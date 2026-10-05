package io.atlas.payments.core

import java.time.Instant
import java.util.UUID

/**
 * Persistence contracts. Production implementations live in
 * [io.atlas.payments.db] (Exposed + Postgres). Tests use the in-memory fakes
 * so [PaymentsService] can be exercised without a database.
 */

/**
 * Every method takes projectId FIRST rather than as an optional filter, so
 * an unscoped query cannot be written by omission — the signature asks on
 * every call.
 */
interface WalletRepository {
    /** Returns the wallet for [userId], creating a zero-balance one if absent. */
    fun getOrCreateByUser(projectId: UUID, userId: UUID): Wallet
    fun findByUser(projectId: UUID, userId: UUID): Wallet?
    fun findById(projectId: UUID, id: UUID): Wallet?
    /** Adds [deltaCents] (which may be negative) to the wallet balance. */
    fun adjustBalance(projectId: UUID, walletId: UUID, deltaCents: Long)
}

interface TransactionRepository {
    /**
     * Scoped, and this one is not merely defensive.
     *
     * Idempotency keys are chosen by the caller — "order-1" is the string
     * every integration picks first — and the unique index is
     * (project_id, idempotency_key). A lookup by key alone would match
     * another tenant's row and return it as a successful idempotent
     * replay: not an error, someone else's payment.
     */
    fun findByIdempotencyKey(projectId: UUID, key: String): TxRecord?
    fun findById(projectId: UUID, id: UUID): TxRecord?

    /**
     * Find a transaction by its provider reference — the webhook path.
     *
     * A Stripe event names a PaymentIntent id, and the pending row holds
     * it in provider_ref. Scoped by project for the same reason every
     * other read here is: the webhook endpoint is per-tenant
     * (/webhooks/stripe/{project_id}), so the lookup must not be able to
     * match another tenant's charge.
     */
    fun findByProviderRef(projectId: UUID, providerRef: String): TxRecord?
    /** Inserts a pending transaction. Throws [DuplicateIdempotencyKey] on conflict. */
    fun insertPending(
        projectId: UUID,
        fromWallet: UUID?,
        toWallet: UUID?,
        amountCents: Long,
        idempotencyKey: String,
        rideId: UUID?,
        providerRef: String?,
        argsHash: String?,
        /** One of [TxKind]. Defaulted so transfer call sites need no change. */
        kind: String = TxKind.TRANSFER,
    ): TxRecord
    fun markSettled(projectId: UUID, id: UUID, settledAt: Instant)
    fun markRefunded(projectId: UUID, id: UUID)

    /**
     * Mark a transaction failed.
     *
     * Needed by the deposit path: when the provider declines the capture
     * of an already-recorded pending deposit, the row must not be left
     * pending forever, where the reconciliation sweep would keep
     * retrying a charge the provider has already refused.
     */
    fun markFailed(projectId: UUID, id: UUID, reason: String)

    /**
     * Transactions still PENDING that were created before [cutoff],
     * oldest first, across ALL projects.
     *
     * Deliberately not project-scoped: this is an operator-facing sweep,
     * not a customer request. Scoping it would mean either running it once
     * per tenant — a query per project per pass — or having an operator
     * remember to. Each row carries its own project and every write the
     * sweep makes is scoped by it.
     */
    fun findStuckPending(cutoff: Instant, limit: Int): List<TxRecord>
}

/**
 * Write side of the transactional outbox. [enqueue] is called from inside the
 * same transaction as the wallet/transaction mutations so the event and the
 * state change commit atomically.
 */
interface OutboxStore {
    fun enqueue(transactionId: UUID?, topic: String, payload: ByteArray)
}
// payments.outbox needs no project column: the dispatcher drains it
// globally by age rather than per tenant, each row cascades from its
// transaction, and the payload itself now carries project_id.

/**
 * Drain side of the outbox. [drain] claims up to [limit] pending rows, calls
 * [publish] for each, and marks them dispatched (or records the failure) - all
 * within a single transaction so the FOR UPDATE SKIP LOCKED claim holds until
 * commit. Returns the number successfully published.
 */
interface OutboxBackend {
    fun drain(limit: Int, publish: (OutboxRow) -> Unit): Int

    /**
     * How many rows are still waiting, and how old the oldest one is in
     * seconds (0 when there are none).
     *
     * This is the signal that matters for payments. `outboxDispatched` is
     * a counter of SUCCESSES: when Kafka is unreachable it simply stops
     * increasing, and a counter that stops is indistinguishable from a
     * system with nothing to do. Depth and age go UP when the drain is
     * stuck, which is a statement rather than an absence — and a stuck
     * outbox means settlements and refunds are not happening.
     */
    fun pending(): OutboxDepth
}

data class OutboxDepth(val rows: Long, val oldestAgeSeconds: Long)

/**
 * Runs a block inside a single database transaction so a group of repository
 * calls commit atomically. Production wraps Exposed's `transaction {}`; tests
 * run the block directly. Repository methods join the ambient transaction.
 */
interface TransactionRunner {
    fun <T> run(block: () -> T): T
}
