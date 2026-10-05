package io.atlas.payments.core

import org.slf4j.LoggerFactory
import java.time.Clock
import java.util.UUID

/**
 * Applies a settlement's balance movement, in one transaction with the
 * status change.
 *
 * Extracted from ReconciliationSweep because two independent paths must
 * now produce byte-identical money movement: the sweep resolving a stuck
 * row, and the webhook recording a capture the caller's HTTP response
 * never confirmed. Sharing anything looser than one object would let the
 * two halves drift until a webhook-settled transaction and a
 * sweep-settled one move money differently — the kind of divergence that
 * only surfaces in a reconciliation report nobody enjoys writing.
 */
class SettlementApplier(
    private val wallets: WalletRepository,
    private val transactions: TransactionRepository,
    private val runner: TransactionRunner,
    private val outbox: OutboxStore,
    private val fareTopic: String,
    private val clock: Clock = Clock.systemUTC(),
) {
    /**
     * Move the balances and mark the transaction settled. Idempotent at
     * the caller's discretion: callers re-check [TxRecord.status] under
     * whatever lock they hold before calling.
     */
    fun applySettlement(tx: TxRecord) {
        runner.run {
            tx.fromWallet?.let { wallets.adjustBalance(tx.projectId, it, -tx.amountCents) }
            tx.toWallet?.let { wallets.adjustBalance(tx.projectId, it, tx.amountCents) }
            transactions.markSettled(tx.projectId, tx.id, clock.instant())
            outbox.enqueue(
                tx.id,
                fareTopic,
                fareEvent(tx),
            )
        }
        LOG.info("settled transaction {} ({} cents)", tx.id, tx.amountCents)
    }

    /**
     * Reverse a settlement: balances back, status REFUNDED, event emitted.
     */
    fun applyRefund(tx: TxRecord) {
        runner.run {
            tx.fromWallet?.let { wallets.adjustBalance(tx.projectId, it, tx.amountCents) }
            tx.toWallet?.let { wallets.adjustBalance(tx.projectId, it, -tx.amountCents) }
            transactions.markRefunded(tx.projectId, tx.id)
            outbox.enqueue(tx.id, fareTopic, fareEvent(tx, refunded = true))
        }
        LOG.info("refunded transaction {}", tx.id)
    }

    private fun fareEvent(tx: TxRecord, refunded: Boolean = false): ByteArray =
        atlas.events.FareEvent.newBuilder()
            .setProjectId(tx.projectId.toString())
            .setRideId(tx.rideId?.toString() ?: "")
            .setTransactionId(tx.id.toString())
            .setEventType(
                if (refunded) atlas.events.FareEvent.EventType.TRANSACTION_REFUNDED
                else atlas.events.FareEvent.EventType.TRANSACTION_SETTLED,
            )
            .setAmountCents(tx.amountCents)
            .setOccurredAt(clock.instant().epochSecond)
            .build()
            .toByteArray()

    private companion object {
        private val LOG = LoggerFactory.getLogger(SettlementApplier::class.java)
    }
}

/**
 * Handles verified Stripe webhook events for one project.
 *
 * # The contract with the HTTP layer
 *
 * The endpoint verifies the signature and hands the project and RAW body
 * here; this class does the acting. Anything it cannot act on returns
 * [Result.Ignored] rather than throwing, because a 500 to Stripe means
 * "retry this event forever" — and an event Atlas cannot act on is, from
 * Stripe's point of view, successfully delivered.
 *
 * # Why only three event types
 *
 * `payment_intent.succeeded` and `payment_intent.amount_capturable_updated`
 * both mean "captured, wallet not credited yet" (the second covers the
 * manual-capture flow where a webhook settles a capture the RPC response
 * lost). `charge.refunded` reverses. Everything else —
 * `payment_intent.payment_failed`, disputes, balance events — is either
 * already handled synchronously (a failed authorize never writes a row)
 * or is about money Atlas does not hold (Stripe's balance).
 */
class StripeWebhookHandler(
    private val projectId: UUID,
    private val transactions: TransactionRepository,
    private val wallets: WalletRepository,
    private val runner: TransactionRunner,
    private val outbox: OutboxStore,
    private val fareTopic: String,
    private val payoutAccounts: PayoutAccountSource,
    private val clock: Clock = Clock.systemUTC(),
) {
    sealed class Result {
        /** State changed; balances moved. */
        data class Applied(val what: String) : Result()

        /** Known event, nothing to do — already processed, or not ours. */
        data class Ignored(val why: String) : Result()

        /** We could not parse what Stripe sent. Logged, never a 5xx. */
        data class Malformed(val why: String) : Result()
    }

    private val applier = SettlementApplier(wallets, transactions, runner, outbox, fareTopic, clock)

    fun handle(eventType: String, payloadJson: String): Result {
        // Only fields Atlas itself stamped are read (atlas_project_id,
        // atlas_user_id) plus the object id — never a full Stripe object
        // model, which changes under us without notice. Deliberately regex
        // rather than a JSON parser: the signature already guarantees the
        // bytes came from Stripe, the patterns anchor on literals only we
        // write, and a parser would pull a JSON dependency into the
        // webhook path for three string extractions.
        val intentId = extractString(payloadJson, """"id"\s*:\s*"(pi_[^"]+)"""")
            ?: return Result.Malformed("no payment intent id in payload")

        return when (eventType) {
            "payment_intent.succeeded",
            "payment_intent.amount_capturable_updated",
            -> settleByProviderRef(intentId)
            "charge.refunded" -> refundByProviderRef(intentId)
            "account.updated" -> updatePayoutAccount(payloadJson)
            else -> Result.Ignored("event type $eventType is not acted on")
        }
    }

    private fun settleByProviderRef(intentId: String): Result {
        val tx = transactions.findByProviderRef(projectId, intentId)
            ?: return Result.Ignored("no transaction for $intentId in this project")
        return when (tx.status) {
            TxStatus.SETTLED -> Result.Ignored("${tx.id} already settled (duplicate delivery)")
            TxStatus.PENDING -> {
                applier.applySettlement(tx)
                Result.Applied("settled ${tx.id}")
            }
            else -> Result.Ignored("${tx.id} is ${tx.status}; webhook settlement does not apply")
        }
    }

    private fun refundByProviderRef(intentId: String): Result {
        val tx = transactions.findByProviderRef(projectId, intentId)
            ?: return Result.Ignored("no transaction for $intentId in this project")
        return when (tx.status) {
            TxStatus.REFUNDED -> Result.Ignored("${tx.id} already refunded (duplicate delivery)")
            TxStatus.SETTLED -> {
                applier.applyRefund(tx)
                Result.Applied("refunded ${tx.id}")
            }
            else -> Result.Ignored("${tx.id} is ${tx.status}; only a settled transaction can be refunded")
        }
    }

    private fun updatePayoutAccount(payloadJson: String): Result {
        val accountId = extractString(payloadJson, """"id"\s*:\s*"(acct_[^"]+)"""")
            ?: return Result.Malformed("account.updated without an account id")
        val userId = extractString(payloadJson, """"atlas_user_id"\s*:\s*"([^"]+)"""")
            ?: return Result.Ignored("account $accountId carries no atlas_user_id; not ours")
        val enabled = payloadJson.contains(""""payouts_enabled"\s*:\s*true""")
        payoutAccounts.setPayoutsEnabled(projectId, parse(userId), accountId, enabled)
        return Result.Applied("account $accountId payouts_enabled=$enabled")
    }

    private fun parse(s: String): UUID =
        try {
            UUID.fromString(s)
        } catch (e: IllegalArgumentException) {
            throw IllegalStateException("atlas_user_id metadata was not a UUID: $s")
        }

    private fun extractString(json: String, pattern: String): String? =
        Regex(pattern).find(json)?.groupValues?.getOrNull(1)
}
