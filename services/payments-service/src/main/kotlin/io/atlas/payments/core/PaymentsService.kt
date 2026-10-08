package io.atlas.payments.core

import atlas.events.FareEvent
import org.slf4j.LoggerFactory
import java.time.Clock
import java.util.UUID

/**
 * Payment business logic, free of any gRPC or persistence detail.
 *
 * The money-moving invariant: every transaction state change writes a
 * [atlas.events.FareEvent] to the outbox in the SAME [TransactionRunner.run]
 * block as the wallet and transaction mutations, so the event and the
 * state change commit atomically. The background dispatcher later drains
 * the outbox to Kafka.
 *
 * Provider calls happen OUTSIDE the transaction: network I/O must never
 * hold a Postgres row lock.
 *
 * Lifecycle:
 *   Deposit    -> provider.authorize + capture, credit wallet, TRANSACTION_SETTLED
 *   Initiate   -> provider.authorize, pending tx, client_secret to the caller
 *   Settle     -> provider.capture, apply via SettlementApplier
 *   Refund     -> provider.refund on SETTLED; provider.cancel on PENDING
 *
 * Under Stripe Connect a card-funded fare never debits the payer's wallet
 * (D2): the card pays, the wallet mirrors.
 */
class PaymentsService(
    private val wallets: WalletRepository,
    private val transactions: TransactionRepository,
    private val outbox: OutboxStore,
    private val runner: TransactionRunner,
    private val provider: PaymentProvider,
    private val fareTopic: String,
    private val payoutAccounts: PayoutAccountSource? = null,
    /**
     * Starts Connect onboarding for a user and returns the hosted URL.
     * Injected rather than taken from [PayoutAccountSource] because the
     * flow is "network call, then persist" — a composite the Stripe
     * onboarding adapter owns, not the read-mostly payout source.
     */
    private val onboardingStarter: ((projectId: UUID, userId: UUID, returnUrl: String) -> String)? = null,
    private val clock: Clock = Clock.systemUTC(),
    private val metrics: PaymentsMetrics = PaymentsMetrics.NOOP,
) {
    private val applier =
        SettlementApplier(wallets, transactions, runner, outbox, fareTopic, clock)

    data class DepositResult(val transactionId: UUID, val status: String, val balanceCents: Long)
    data class InitiateResult(val transactionId: UUID, val status: String, val clientSecret: String?)
    data class SettleResult(val success: Boolean, val status: String)
    data class RefundResult(val success: Boolean)
    data class WalletBalance(val balanceCents: Long, val currency: String)

    /**
     * Add funds to a user's wallet from an external payment method.
     *
     * The sequence is authorize -> record a pending row -> capture ->
     * credit and settle. The pending row is written BEFORE the capture on
     * purpose: capture is the step that takes the customer's money, so a
     * crash right after it leaves a row on disk with its provider_ref for
     * the sweep to finish against. Capture-first would leave a charged
     * card with no record of it anywhere.
     */
    fun deposit(
        projectId: UUID,
        userId: String,
        amountCents: Long,
        idempotencyKey: String,
    ): DepositResult {
        if (amountCents <= 0) throw PaymentError.InvalidAmount(amountCents)
        if (idempotencyKey.isBlank()) throw PaymentError.InvalidArgument("idempotency_key is required")
        val userUuid = parseUuid(userId, "user_id")

        val argsHash = idempotencyArgsHash(userId, "", amountCents, "", 0)

        transactions.findByIdempotencyKey(projectId, idempotencyKey)?.let { existing ->
            if (!idempotencyArgsMatch(existing.idempotencyArgsHash, userId, "", amountCents, "", 0)) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return DepositResult(
                existing.id,
                existing.status,
                walletBalance(projectId, userId).balanceCents,
            )
        }

        val auth = provider.authorize(
            ChargeRequest(
                projectId = projectId,
                userId = userUuid,
                amountCents = amountCents,
            ),
            idempotencyKey,
        )
        if (!auth.success) throw PaymentError.ProviderDeclined(auth.message ?: "authorize declined")

        val pending = try {
            runner.run {
                val wallet = wallets.getOrCreateByUser(projectId, userUuid)
                transactions.insertPending(
                    projectId,
                    fromWallet = null,
                    toWallet = wallet.id,
                    amountCents = amountCents,
                    idempotencyKey = idempotencyKey,
                    rideId = null,
                    providerRef = auth.providerRef,
                    argsHash = argsHash,
                    kind = TxKind.DEPOSIT,
                )
            }
        } catch (e: DuplicateIdempotencyKey) {
            val winner = transactions.findByIdempotencyKey(projectId, idempotencyKey) ?: throw e
            if (!idempotencyArgsMatch(winner.idempotencyArgsHash, userId, "", amountCents, "", 0)) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return DepositResult(winner.id, winner.status, walletBalance(projectId, userId).balanceCents)
        }

        val capture = provider.capture(projectId, auth.providerRef)
        if (!capture.success) {
            // The charge was refused; leaving the row pending would have
            // the sweep retry a capture the provider already declined.
            transactions.markFailed(projectId, pending.id, capture.message ?: "capture declined")
            metrics.depositFailed()
            throw PaymentError.ProviderDeclined(capture.message ?: "capture declined")
        }

        val balance = runner.run {
            val walletId = pending.toWallet
                ?: throw PaymentError.InvalidState("deposit has no destination wallet")
            if (transactions.transitionSettled(projectId, pending.id, clock.instant()) == 0) {
                // A concurrent delivery (webhook or sweep) already settled.
                return@run wallets.findById(projectId, walletId)?.balanceCents ?: amountCents
            }
            wallets.adjustBalance(projectId, walletId, amountCents)
            outbox.enqueue(
                pending.id,
                fareTopic,
                fareEvent(projectId, "", pending.id, FareEvent.EventType.TRANSACTION_SETTLED, amountCents),
            )
            wallets.findById(projectId, walletId)?.balanceCents ?: amountCents
        }

        metrics.depositSettled()
        LOG.info("deposit settled tx={} amount={}", pending.id, amountCents)
        return DepositResult(pending.id, TxStatus.SETTLED, balance)
    }

    fun initiate(
        projectId: UUID,
        fromUserId: String,
        toUserId: String,
        amountCents: Long,
        idempotencyKey: String,
        rideId: String,
        applicationFeeCents: Long = 0,
    ): InitiateResult {
        if (amountCents <= 0) throw PaymentError.InvalidAmount(amountCents)
        if (applicationFeeCents < 0) {
            throw PaymentError.InvalidArgument("application_fee_cents must not be negative")
        }
        // A fee taken out of the amount must leave the payee something;
        // Stripe rejects fee >= amount.
        if (applicationFeeCents >= amountCents) {
            throw PaymentError.InvalidArgument(
                "application_fee_cents ($applicationFeeCents) must be less than amount_cents ($amountCents)",
            )
        }
        if (idempotencyKey.isBlank()) throw PaymentError.InvalidArgument("idempotency_key is required")
        val fromUuid = parseUuid(fromUserId, "from_user_id")
        val toUuid = parseUuid(toUserId, "to_user_id")
        if (fromUuid == toUuid) {
            throw PaymentError.InvalidArgument("from_user_id and to_user_id must differ")
        }
        val rideUuid = if (rideId.isBlank()) null else parseUuid(rideId, "ride_id")
        val argsHash = idempotencyArgsHash(fromUserId, toUserId, amountCents, rideId, applicationFeeCents)

        transactions.findByIdempotencyKey(projectId, idempotencyKey)?.let { existing ->
            if (!idempotencyArgsMatch(
                    existing.idempotencyArgsHash, fromUserId, toUserId, amountCents, rideId, applicationFeeCents,
                )
            ) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return InitiateResult(existing.id, existing.status, clientSecretFor(existing))
        }

        // Resolve the destination BEFORE authorizing. When Connect
        // onboarding is wired, a payee with no usable account is an error
        // the caller must fix before a charge is placed — not a plain
        // charge the provider would happily take money for.
        val destination = destinationAccountOf(projectId, toUuid)
            ?: if (payoutAccounts != null) throw PaymentError.DriverNotOnboarded(toUuid) else null

        val auth = provider.authorize(
            ChargeRequest(
                projectId = projectId,
                userId = fromUuid,
                amountCents = amountCents,
                destinationAccountId = destination,
                applicationFeeCents = applicationFeeCents.takeIf { it > 0 },
            ),
            idempotencyKey,
        )
        if (!auth.success) throw PaymentError.ProviderDeclined(auth.message ?: "authorize declined")

        val txId = try {
            runner.run {
                val fromWallet = wallets.getOrCreateByUser(projectId, fromUuid)
                val toWallet = wallets.getOrCreateByUser(projectId, toUuid)
                val record = transactions.insertPending(
                    projectId,
                    fromWallet = fromWallet.id,
                    toWallet = toWallet.id,
                    amountCents = amountCents,
                    idempotencyKey = idempotencyKey,
                    rideId = rideUuid,
                    providerRef = auth.providerRef,
                    argsHash = argsHash,
                    cardFunded = destination != null,
                )
                outbox.enqueue(
                    record.id,
                    fareTopic,
                    fareEvent(projectId, rideId, record.id, FareEvent.EventType.RIDE_ACCEPTED, amountCents),
                )
                record.id
            }
        } catch (e: DuplicateIdempotencyKey) {
            // Lost a race with a concurrent identical request; re-read the
            // winner in a FRESH transaction (the aborted one cannot query).
            val winner = transactions.findByIdempotencyKey(projectId, idempotencyKey)
                ?: throw e
            if (!idempotencyArgsMatch(
                    winner.idempotencyArgsHash, fromUserId, toUserId, amountCents, rideId, applicationFeeCents,
                )
            ) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return InitiateResult(winner.id, winner.status, clientSecretFor(winner))
        }
        metrics.transactionInitiated()
        return InitiateResult(txId, TxStatus.PENDING, auth.clientSecret)
    }

    fun settle(projectId: UUID, transactionId: String): SettleResult {
        val txUuid = parseUuid(transactionId, "transaction_id")
        val tx = transactions.findById(projectId, txUuid)
            ?: throw PaymentError.TransactionNotFound(transactionId)
        when (tx.status) {
            TxStatus.SETTLED -> return SettleResult(true, TxStatus.SETTLED) // idempotent
            TxStatus.PENDING -> Unit
            else -> throw PaymentError.InvalidState("cannot settle a ${tx.status} transaction")
        }

        // Deposit-funded transfers: verify the balance BEFORE any provider
        // call, under the row lock the settle transaction takes. Card-funded
        // fares skip this; the card pays (D2).
        if (!tx.cardFunded && tx.fromWallet != null) {
            runner.run {
                val fromWallet = wallets.findById(projectId, tx.fromWallet!!)
                    ?: throw PaymentError.InvalidState("source wallet not found")
                if (fromWallet.balanceCents < tx.amountCents) {
                    throw PaymentError.InsufficientFunds(fromWallet.id)
                }
            }
        }

        val capture = provider.capture(projectId, tx.providerRef ?: "")
        if (!capture.success) throw PaymentError.ProviderDeclined(capture.message ?: "capture declined")

        if (!applier.applySettlement(tx)) {
            // A webhook or the sweep settled it between our read and the
            // CAS. Not an error: the money moved exactly once.
            return SettleResult(true, TxStatus.SETTLED)
        }
        metrics.transactionSettled()
        return SettleResult(true, TxStatus.SETTLED)
    }

    fun refund(projectId: UUID, transactionId: String): RefundResult {
        val txUuid = parseUuid(transactionId, "transaction_id")
        val tx = transactions.findById(projectId, txUuid)
            ?: throw PaymentError.TransactionNotFound(transactionId)
        return when (tx.status) {
            TxStatus.REFUNDED -> RefundResult(true) // idempotent
            TxStatus.SETTLED -> refundSettled(tx)
            TxStatus.PENDING -> cancelPending(tx)
            else -> throw PaymentError.InvalidState(
                "cannot refund a ${tx.status} transaction",
            )
        }
    }

    /** SETTLED -> REFUNDED: reverse the captured money. */
    private fun refundSettled(tx: TxRecord): RefundResult {
        val refund = provider.refund(tx.projectId, tx.providerRef ?: "")
        if (!refund.success) throw PaymentError.ProviderDeclined(refund.message ?: "refund declined")
        if (!applier.applyRefund(tx)) {
            // Concurrent delivery already reversed it.
            return RefundResult(true)
        }
        metrics.transactionRefunded()
        return RefundResult(true)
    }

    /**
     * PENDING -> CANCELLED: the ride died before capture, so nothing was
     * taken. The provider call voids the authorization, releasing the hold.
     */
    private fun cancelPending(tx: TxRecord): RefundResult {
        tx.providerRef?.takeIf { it.isNotBlank() }?.let { ref ->
            val cancel = provider.cancel(tx.projectId, ref)
            if (!cancel.success) {
                throw PaymentError.ProviderDeclined(cancel.message ?: "cancel declined")
            }
        }
        if (!applier.applyCancellation(tx, "ride cancelled before capture")) {
            return RefundResult(true)
        }
        metrics.transactionCancelled()
        return RefundResult(true)
    }

    fun walletBalance(projectId: UUID, userId: String): WalletBalance {
        val uuid = parseUuid(userId, "user_id")
        val wallet = wallets.findByUser(projectId, uuid)
        return if (wallet == null) {
            WalletBalance(0, "USD")
        } else {
            WalletBalance(wallet.balanceCents, wallet.currency)
        }
    }

    // --- Connect onboarding ------------------------------------------------

    /**
     * The caller's payout account, for the app to decide whether to show
     * "add payout account" or "payouts ready".
     */
    fun connectAccountStatus(projectId: UUID, userId: String): ConnectAccountStatus {
        val uuid = parseUuid(userId, "user_id")
        val accounts = payoutAccounts
            ?: return ConnectAccountStatus(exists = false, payoutsEnabled = false)
        val account = accounts.accountFor(projectId, uuid)
        return ConnectAccountStatus(
            exists = account != null,
            payoutsEnabled = account?.payoutsEnabled ?: false,
        )
    }

    data class ConnectAccountStatus(val exists: Boolean, val payoutsEnabled: Boolean)

    /**
     * Create (or resume) the caller's connected payout account and return
     * Stripe's hosted onboarding URL. The Stripe call happens BEFORE any
     * database write: a crash between the two orphans an unused Stripe
     * account, while the reverse order would leave a row pointing at an
     * account Stripe never made.
     */
    fun startConnectOnboarding(projectId: UUID, userId: String, returnUrl: String): String {
        val uuid = parseUuid(userId, "user_id")
        val starter = onboardingStarter
            ?: throw PaymentError.InvalidState(
                "this deployment does not support Connect onboarding",
            )
        return starter(projectId, uuid, returnUrl).also {
            LOG.info("connect onboarding started for user={} project={}", userId, projectId)
        }
    }

    // --- helpers ----------------------------------------------------------

    /**
     * The stored client_secret for a replayed initiation. None for rows
     * created before client-side confirmation existed.
     */
    private fun clientSecretFor(tx: TxRecord): String? {
        if (tx.providerRef.isNullOrBlank()) return null
        return try {
            provider.clientSecret(tx.projectId, tx.providerRef)
        } catch (e: Exception) {
            LOG.warn("could not re-fetch client_secret for {}: {}", tx.id, e.message)
            null
        }
    }

    /**
     * The payee's connected payout account, when Connect onboarding is
     * wired. Null means "no destination" — a plain charge, which is
     * correct for the fake provider; with a real payout source the
     * caller has already turned a missing account into DriverNotOnboarded.
     */
    private fun destinationAccountOf(projectId: UUID, toUuid: UUID): String? {
        val accounts = payoutAccounts ?: return null
        val account = accounts.accountFor(projectId, toUuid) ?: return null
        if (!account.payoutsEnabled) return null
        return account.stripeAccountId
    }

    private fun fareEvent(
        projectId: UUID,
        rideId: String,
        transactionId: UUID,
        type: FareEvent.EventType,
        amountCents: Long,
    ): ByteArray =
        FareEvent.newBuilder()
            .setProjectId(projectId.toString())
            .setRideId(rideId)
            .setTransactionId(transactionId.toString())
            .setEventType(type)
            .setAmountCents(amountCents)
            .setOccurredAt(clock.instant().epochSecond)
            .build()
            .toByteArray()

    private fun parseUuid(value: String, field: String): UUID =
        try {
            UUID.fromString(value)
        } catch (e: IllegalArgumentException) {
            throw PaymentError.InvalidArgument("$field is not a valid UUID: $value")
        }

    companion object {
        private val LOG = LoggerFactory.getLogger(PaymentsService::class.java)
    }
}
