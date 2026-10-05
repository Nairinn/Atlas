package io.atlas.payments.fakes

import io.atlas.payments.core.DuplicateIdempotencyKey
import io.atlas.payments.core.OutboxBackend
import io.atlas.payments.core.OutboxDepth
import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.OutboxRow
import io.atlas.payments.core.OutboxStore
import io.atlas.payments.core.TransactionRepository
import io.atlas.payments.core.TransactionRunner
import io.atlas.payments.core.TxRecord
import io.atlas.payments.core.TxStatus
import io.atlas.payments.core.Wallet
import io.atlas.payments.core.WalletRepository
import java.time.Instant
import java.util.UUID

/**
 * In-memory repository fakes so [io.atlas.payments.core.PaymentsService] and the
 * [io.atlas.payments.outbox.OutboxDispatcher] can be unit-tested without a
 * database. They mirror the semantics of the Exposed implementations.
 */

/** Runs the block directly; tests do not need real transaction isolation. */
class DirectTransactionRunner : TransactionRunner {
    override fun <T> run(block: () -> T): T = block()
}

/**
 * Keyed by (project, user) and (project, wallet) rather than by user or
 * wallet alone.
 *
 * A fake that enforces a weaker rule than the database lets a scoping bug
 * pass in tests and fail in production, which is the wrong way round: the
 * whole value of a test double is that it says no in the same places the
 * real thing does.
 */
class InMemoryWalletRepository : WalletRepository {
    private val byId = linkedMapOf<Pair<UUID, UUID>, Wallet>()
    private val userToId = linkedMapOf<Pair<UUID, UUID>, UUID>()

    /**
     * Users known to belong to a project.
     *
     * The real repository learns this from the composite (project_id,
     * user_id) foreign key added by migration 0080. A fake with no notion
     * of membership would happily create a wallet for anybody, which is
     * precisely the bug that constraint exists to stop — so the fake
     * models it rather than being more permissive than the schema.
     *
     * Empty means "no membership recorded", and for the many tests that
     * predate this the fake stays permissive; [registerMember] opts a test
     * into the strict behaviour.
     */
    private val members = linkedMapOf<UUID, MutableSet<UUID>>()

    /** Declare that [userId] is a member of [projectId]. */
    @Synchronized
    fun registerMember(projectId: UUID, userId: UUID) {
        members.getOrPut(projectId) { mutableSetOf() }.add(userId)
    }

    @Synchronized
    override fun getOrCreateByUser(projectId: UUID, userId: UUID): Wallet {
        val known = members[projectId]
        if (known != null && userId !in known) {
            throw PaymentError.UnknownUser(userId)
        }
        userToId[projectId to userId]?.let { return byId.getValue(projectId to it) }
        val wallet = Wallet(UUID.randomUUID(), userId, 0, "USD")
        byId[projectId to wallet.id] = wallet
        userToId[projectId to userId] = wallet.id
        return wallet
    }

    @Synchronized
    override fun findByUser(projectId: UUID, userId: UUID): Wallet? =
        userToId[projectId to userId]?.let { byId[projectId to it] }

    @Synchronized
    override fun findById(projectId: UUID, id: UUID): Wallet? = byId[projectId to id]

    @Synchronized
    override fun adjustBalance(projectId: UUID, walletId: UUID, deltaCents: Long) {
        val wallet = byId.getValue(projectId to walletId)
        byId[projectId to walletId] = wallet.copy(balanceCents = wallet.balanceCents + deltaCents)
    }
}

class InMemoryTransactionRepository : TransactionRepository {
    private val byId = linkedMapOf<Pair<UUID, UUID>, TxRecord>()

    /** (project, key), matching `transactions_project_idempotency_key`. */
    private val keyToId = linkedMapOf<Pair<UUID, String>, UUID>()

    @Synchronized
    override fun findByIdempotencyKey(projectId: UUID, key: String): TxRecord? =
        keyToId[projectId to key]?.let { byId[projectId to it] }

    @Synchronized
    override fun findById(projectId: UUID, id: UUID): TxRecord? = byId[projectId to id]

    @Synchronized
    override fun findByProviderRef(projectId: UUID, providerRef: String): TxRecord? =
        byId.entries
            .filter { (key, _) -> key.first == projectId }
            .map { (_, record) -> record }
            .firstOrNull { it.providerRef == providerRef }

    @Synchronized
    override fun insertPending(
        projectId: UUID,
        fromWallet: UUID?,
        toWallet: UUID?,
        amountCents: Long,
        idempotencyKey: String,
        rideId: UUID?,
        providerRef: String?,
        argsHash: String?,
        kind: String,
    ): TxRecord {
        if (keyToId.containsKey(projectId to idempotencyKey)) {
            throw DuplicateIdempotencyKey(idempotencyKey)
        }
        val record = TxRecord(
            id = UUID.randomUUID(),
            projectId = projectId,
            fromWallet = fromWallet,
            toWallet = toWallet,
            amountCents = amountCents,
            status = TxStatus.PENDING,
            idempotencyKey = idempotencyKey,
            rideId = rideId,
            providerRef = providerRef,
            idempotencyArgsHash = argsHash,
            kind = kind,
        )
        byId[projectId to record.id] = record
        keyToId[projectId to idempotencyKey] = record.id
        createdAt[record.id] = Instant.now()
        return record
    }

    @Synchronized
    override fun markSettled(projectId: UUID, id: UUID, settledAt: Instant) {
        byId[projectId to id] = byId.getValue(projectId to id).copy(status = TxStatus.SETTLED)
    }

    @Synchronized
    override fun markRefunded(projectId: UUID, id: UUID) {
        byId[projectId to id] = byId.getValue(projectId to id).copy(status = TxStatus.REFUNDED)
    }

    /** When each transaction was created, for the reconciliation sweep. */
    private val createdAt = linkedMapOf<UUID, Instant>()

    /**
     * Pretend a transaction was created at [at].
     *
     * Tests need rows older than the sweep's threshold without waiting
     * fifteen minutes, and a clock-injected repository would be a larger
     * fake than the thing it stands in for.
     */
    @Synchronized
    fun backdate(id: UUID, at: Instant) {
        createdAt[id] = at
    }

    @Synchronized
    override fun findStuckPending(cutoff: Instant, limit: Int): List<TxRecord> =
        byId.values
            .filter { it.status == TxStatus.PENDING }
            .filter { (createdAt[it.id] ?: Instant.now()) < cutoff }
            .sortedBy { createdAt[it.id] ?: Instant.now() }
            .take(limit)

    @Synchronized
    override fun markFailed(projectId: UUID, id: UUID, reason: String) {
        byId[projectId to id] = byId.getValue(projectId to id).copy(status = TxStatus.FAILED)
    }
}

/**
 * In-memory outbox implementing both the write side ([OutboxStore]) and the
 * drain side ([OutboxBackend]) over one shared list, so a test can enqueue via
 * the service and drain via the dispatcher.
 */
class InMemoryOutbox : OutboxStore, OutboxBackend {
    private val rows = mutableListOf<OutboxRow>()
    private val dispatched = mutableSetOf<UUID>()
    private val attempts = linkedMapOf<UUID, Int>()

    @Synchronized
    override fun enqueue(transactionId: UUID?, topic: String, payload: ByteArray) {
        rows += OutboxRow(UUID.randomUUID(), transactionId, topic, payload)
    }

    @Synchronized
    override fun drain(limit: Int, publish: (OutboxRow) -> Unit): Int {
        val pending = rows.filter { it.id !in dispatched }.take(limit)
        var count = 0
        for (row in pending) {
            try {
                publish(row)
                dispatched += row.id
                count++
            } catch (e: Exception) {
                attempts[row.id] = (attempts[row.id] ?: 0) + 1
            }
        }
        return count
    }

    /**
     * The port's depth query. Age is always 0 here: this fake has no
     * clock and the tests that use it assert on the count, so inventing
     * timestamps would add a moving part without adding a guarantee.
     */
    @Synchronized
    override fun pending(): OutboxDepth = OutboxDepth(pendingCount().toLong(), 0)

    @Synchronized
    fun pendingCount(): Int = rows.count { it.id !in dispatched }

    @Synchronized
    fun all(): List<OutboxRow> = rows.toList()

    @Synchronized
    fun attemptsFor(id: UUID): Int = attempts[id] ?: 0
}
