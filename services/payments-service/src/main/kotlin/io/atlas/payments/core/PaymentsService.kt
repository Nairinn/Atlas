package io.atlas.payments.core

import atlas.events.FareEvent
import org.slf4j.LoggerFactory
import java.time.Clock
import java.util.UUID

/**
 * Payment business logic, free of any gRPC or persistence detail.
 *
 * The money-moving invariant: every transaction state change writes a
 * [FareEvent] to the outbox in the SAME [TransactionRunner.run] block as the
 * wallet and transaction mutations, so the event and the state change commit
 * atomically. The background dispatcher later drains the outbox to Kafka.
 *
 * Provider calls (authorize/capture/refund) happen OUTSIDE the transaction:
 * network I/O must never hold a Postgres row lock.
 *
 * Lifecycle:
 *   Deposit             -> provider.authorize + capture, credit wallet, TRANSACTION_SETTLED
 *   InitiateTransaction -> provider.authorize, pending tx, RIDE_ACCEPTED event
 *   SettleTransaction   -> provider.capture, move balances, TRANSACTION_SETTLED
 *   RefundTransaction   -> provider.refund, reverse balances, TRANSACTION_REFUNDED
 *
 * Deposit is the only way money enters the system. Without it every wallet
 * sits at zero and [settle] refuses to move funds that are not there, which
 * made the whole service unusable end to end.
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
    data class DepositResult(val transactionId: UUID, val status: String, val balanceCents: Long)
    data class InitiateResult(val transactionId: UUID, val status: String)
    data class SettleResult(val success: Boolean, val status: String)
    data class RefundResult(val success: Boolean)
    data class WalletBalance(val balanceCents: Long, val currency: String)

    /**
     * Add funds to a user's wallet from an external payment method.
     *
     * # Ordering, and the window it leaves open
     *
     * The sequence is: authorize -> record a pending row -> capture ->
     * credit and settle. The pending row is written BEFORE the capture on
     * purpose. Capture is the step that actually takes the customer's
     * money, so if the process dies immediately afterwards the row is
     * already on disk with its `provider_ref`, and a sweep over pending
     * deposits (indexed for exactly this in migration 0033) can ask the
     * provider what happened and finish the job.
     *
     * Reversing the order — capture first, then write — would lose that:
     * a crash would leave a charged card and no record of it anywhere in
     * this system. Money taken with no trace is the one outcome worth
     * contorting the code to avoid.
     *
     * The remaining window is small and recoverable rather than absent,
     * which is the honest position for a system that has to call a
     * network service and write a database in two separate steps.
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

        // A deposit has no counterparty and no ride, so the hash covers the
        // payee and the amount. Reusing a key with a different amount is a
        // conflict, not a silent success returning the wrong transaction.
        val argsHash = idempotencyArgsHash(userId, "", amountCents, "")

        transactions.findByIdempotencyKey(projectId, idempotencyKey)?.let { existing ->
            if (existing.idempotencyArgsHash != argsHash) {
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
                // No destination and no fee: a deposit pays nobody but the
                // depositor, and the tenant has not yet earned a cut of
                // money the user just put in.
            ),
            idempotencyKey,
        )
        if (!auth.success) throw PaymentError.ProviderDeclined(auth.message ?: "authorize declined")

        val pending = try {
            runner.run {
                val wallet = wallets.getOrCreateByUser(projectId, userUuid)
                transactions.insertPending(
                    projectId,
                    // No source wallet: this money comes from outside the
                    // platform. That is what makes it a deposit.
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
            if (winner.idempotencyArgsHash != argsHash) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return DepositResult(winner.id, winner.status, walletBalance(projectId, userId).balanceCents)
        }

        val capture = provider.capture(projectId, auth.providerRef)
        if (!capture.success) {
            // The charge was refused, so leaving the row pending would have
            // the reconciliation sweep retry a capture the provider has
            // already declined.
            transactions.markFailed(projectId, pending.id, capture.message ?: "capture declined")
            metrics.depositFailed()
            throw PaymentError.ProviderDeclined(capture.message ?: "capture declined")
        }

        val balance = runner.run {
            val walletId = pending.toWallet
                ?: throw PaymentError.InvalidState("deposit has no destination wallet")
            wallets.adjustBalance(projectId, walletId, amountCents)
            transactions.markSettled(projectId, pending.id, clock.instant())
            outbox.enqueue(
                pending.id,
                fareTopic,
                // No ride is involved, so ride_id is empty. The event type is
                // TRANSACTION_SETTLED rather than a new one: consumers already
                // treat that as an audit record, and adding an enum value
                // would force every consumer to redeploy before deposits
                // could ship.
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
        // Stripe rejects fee >= amount, and the error it returns names
        // Stripe internals this API does not expose.
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
        val argsHash = idempotencyArgsHash(fromUserId, toUserId, amountCents, rideId)

        // Idempotent replay: same key + same args returns the existing tx.
        transactions.findByIdempotencyKey(projectId, idempotencyKey)?.let { existing ->
            if (existing.idempotencyArgsHash != argsHash) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return InitiateResult(existing.id, existing.status)
        }

        // Authorize against the provider BEFORE opening the db transaction.
        val auth = provider.authorize(
            ChargeRequest(
                projectId = projectId,
                userId = fromUuid,
                amountCents = amountCents,
                // The payee's connected account is what makes this a
                // Connect destination charge rather than a plain transfer:
                // the tenant's Stripe account charges the rider, Stripe
                // moves the driver's share to the destination, and the
                // application fee stays with the tenant. Both are resolved
                // by the provider from the project — the service does not
                // know they are Stripe concepts.
                destinationAccountId = destinationAccountOf(projectId, toUuid),
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
                )
                outbox.enqueue(
                    record.id,
                    fareTopic,
                    fareEvent(projectId, rideId, record.id, FareEvent.EventType.RIDE_ACCEPTED, amountCents),
                )
                record.id
            }
        } catch (e: DuplicateIdempotencyKey) {
            // Lost a race with a concurrent identical request. The duplicate-key
            // violation rolled the whole transaction back, so we re-read the
            // winner in a FRESH transaction (the aborted one cannot run queries).
            val winner = transactions.findByIdempotencyKey(projectId, idempotencyKey)
                ?: throw e
            if (winner.idempotencyArgsHash != argsHash) {
                throw PaymentError.IdempotencyConflict(idempotencyKey)
            }
            return InitiateResult(winner.id, winner.status)
        }
        metrics.transactionInitiated()
        return InitiateResult(txId, TxStatus.PENDING)
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

        val capture = provider.capture(projectId, tx.providerRef ?: "")
        if (!capture.success) throw PaymentError.ProviderDeclined(capture.message ?: "capture declined")

        runner.run {
            val fromWalletId = tx.fromWallet
                ?: throw PaymentError.InvalidState("transaction has no source wallet")
            val fromWallet = wallets.findById(projectId, fromWalletId)
                ?: throw PaymentError.InvalidState("source wallet not found")
            if (fromWallet.balanceCents < tx.amountCents) {
                throw PaymentError.InsufficientFunds(fromWallet.id)
            }
            wallets.adjustBalance(projectId, fromWalletId, -tx.amountCents)
            tx.toWallet?.let { wallets.adjustBalance(projectId, it, tx.amountCents) }
            transactions.markSettled(projectId, txUuid, clock.instant())
            outbox.enqueue(
                txUuid,
                fareTopic,
                fareEvent(projectId, rideRef(tx), txUuid, FareEvent.EventType.TRANSACTION_SETTLED, tx.amountCents),
            )
        }
        metrics.transactionSettled()
        return SettleResult(true, TxStatus.SETTLED)
    }

    fun refund(projectId: UUID, transactionId: String): RefundResult {
        val txUuid = parseUuid(transactionId, "transaction_id")
        val tx = transactions.findById(projectId, txUuid)
            ?: throw PaymentError.TransactionNotFound(transactionId)
        when (tx.status) {
            TxStatus.REFUNDED -> return RefundResult(true) // idempotent
            TxStatus.SETTLED -> Unit
            else -> throw PaymentError.InvalidState("can only refund a settled transaction; status=${tx.status}")
        }

        val refund = provider.refund(projectId, tx.providerRef ?: "")
        if (!refund.success) throw PaymentError.ProviderDeclined(refund.message ?: "refund declined")

        runner.run {
            // Reverse the settle: money flows back from payee to payer.
            tx.fromWallet?.let { wallets.adjustBalance(projectId, it, tx.amountCents) }
            tx.toWallet?.let { wallets.adjustBalance(projectId, it, -tx.amountCents) }
            transactions.markRefunded(projectId, txUuid)
            outbox.enqueue(
                txUuid,
                fareTopic,
                fareEvent(projectId, rideRef(tx), txUuid, FareEvent.EventType.TRANSACTION_REFUNDED, tx.amountCents),
            )
        }
        metrics.transactionRefunded()
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
     * Stripe's hosted onboarding URL.
     *
     * The Stripe call happens BEFORE any database write, deliberately:
     * the network side effects are idempotent in the worst direction. If
     * the process dies after creating the account at Stripe but before
     * saving the row, the next call re-creates — Connect treats a second
     * create for the same user as a new account, which wastes nothing but
     * an unused Stripe account row. The reverse order (row first) would
     * leave a row pointing at nothing, which authorize would then trust.
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
     * The payee's connected payout account, when Connect onboarding is
     * wired. A provider that is not Connect-aware (the fake) needs no
     * destination: null means "plain charge", which is correct for it.
     *
     * A real payee with NO account is NOT null-able here — the caller has
     * named a recipient who cannot be paid, and the provider will refuse
     * with DriverNotOnboarded rather than charging the payer for a ride
     * that can never be settled.
     */
    private fun destinationAccountOf(projectId: UUID, toUuid: UUID): String? {
        val accounts = payoutAccounts ?: return null
        val account = accounts.accountFor(projectId, toUuid) ?: return null
        if (!account.payoutsEnabled) return null
        return account.stripeAccountId
    }

    private fun rideRef(tx: TxRecord): String = tx.rideId?.toString() ?: ""

    /**
     * The event carries its project because the consumer reading it has no
     * other way to learn one: it runs asynchronously, long after the
     * request that produced the event is gone, so there is no header and
     * no token left to derive it from.
     */
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
