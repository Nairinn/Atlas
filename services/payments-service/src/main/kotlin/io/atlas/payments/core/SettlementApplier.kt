package io.atlas.payments.core

import atlas.events.FareEvent
import org.slf4j.LoggerFactory
import java.time.Clock

/**
 * The one implementation of a transaction's terminal money movement:
 * balance changes, the status CAS, and the outbox event, in one DB
 * transaction. Settle, refund, cancel, the webhook handler, and the
 * reconciliation sweep all go through it, so no two paths can drift.
 *
 * Every status flip is a compare-and-set: if it returns 0 someone else
 * already applied the transition, and the balances are left untouched.
 */
class SettlementApplier(
    private val wallets: WalletRepository,
    private val transactions: TransactionRepository,
    private val runner: TransactionRunner,
    private val outbox: OutboxStore,
    private val fareTopic: String,
    private val clock: Clock = Clock.systemUTC(),
) {
    /** PENDING -> SETTLED, moving balances only when the CAS wins. */
    fun applySettlement(tx: TxRecord): Boolean = runner.run {
        if (transactions.transitionSettled(tx.projectId, tx.id, clock.instant()) == 0) {
            return@run false
        }
        // Card-funded fares never touched the payer's wallet (D2): the
        // card pays, so only the mirror ledger on the payee side moves.
        if (!tx.cardFunded) {
            tx.fromWallet?.let { wallets.adjustBalance(tx.projectId, it, -tx.amountCents) }
        }
        tx.toWallet?.let { wallets.adjustBalance(tx.projectId, it, tx.amountCents) }
        outbox.enqueue(tx.id, fareTopic, fareEvent(tx, FareEvent.EventType.TRANSACTION_SETTLED))
        true
    }.also {
        if (it) LOG.info("settled transaction {} ({} cents)", tx.id, tx.amountCents)
    }

    /** SETTLED -> REFUNDED, reversing balances only when the CAS wins. */
    fun applyRefund(tx: TxRecord): Boolean = runner.run {
        if (transactions.transitionRefunded(tx.projectId, tx.id) == 0) {
            return@run false
        }
        if (!tx.cardFunded) {
            tx.fromWallet?.let { wallets.adjustBalance(tx.projectId, it, tx.amountCents) }
        }
        tx.toWallet?.let { wallets.adjustBalance(tx.projectId, it, -tx.amountCents) }
        outbox.enqueue(tx.id, fareTopic, fareEvent(tx, FareEvent.EventType.TRANSACTION_REFUNDED))
        true
    }.also {
        if (it) LOG.info("refunded transaction {}", tx.id)
    }

    /**
     * PENDING -> CANCELLED. No balances move: nothing was captured, the
     * provider call voided only a hold.
     */
    fun applyCancellation(tx: TxRecord, reason: String): Boolean = runner.run {
        if (transactions.transitionCancelled(tx.projectId, tx.id, reason) == 0) {
            return@run false
        }
        outbox.enqueue(tx.id, fareTopic, fareEvent(tx, FareEvent.EventType.TRANSACTION_CANCELLED))
        true
    }.also {
        if (it) LOG.info("cancelled transaction {}: {}", tx.id, reason)
    }

    private fun fareEvent(tx: TxRecord, type: atlas.events.FareEvent.EventType): ByteArray =
        atlas.events.FareEvent.newBuilder()
            .setProjectId(tx.projectId.toString())
            .setRideId(tx.rideId?.toString() ?: "")
            .setTransactionId(tx.id.toString())
            .setEventType(type)
            .setAmountCents(tx.amountCents)
            .setOccurredAt(clock.instant().epochSecond)
            .build()
            .toByteArray()

    private companion object {
        private val LOG = LoggerFactory.getLogger(SettlementApplier::class.java)
    }
}
