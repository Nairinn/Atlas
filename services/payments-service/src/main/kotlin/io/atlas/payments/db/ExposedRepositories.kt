package io.atlas.payments.db

import io.atlas.payments.core.DuplicateIdempotencyKey
import io.atlas.payments.core.TransactionRepository
import io.atlas.payments.core.TransactionRunner
import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.TxRecord
import io.atlas.payments.core.TxStatus
import io.atlas.payments.core.Wallet
import io.atlas.payments.core.WalletRepository
import org.jetbrains.exposed.exceptions.ExposedSQLException
import org.jetbrains.exposed.sql.ResultRow
import org.jetbrains.exposed.sql.SortOrder
import org.jetbrains.exposed.sql.and
import org.jetbrains.exposed.sql.SqlExpressionBuilder.plus
import org.jetbrains.exposed.sql.insert
import org.jetbrains.exposed.sql.selectAll
import org.jetbrains.exposed.sql.transactions.transaction
import org.jetbrains.exposed.sql.update
import java.time.Instant
import java.util.UUID

/**
 * Postgres-backed repositories. Each method opens an Exposed `transaction {}`.
 * Exposed nests these by joining the outermost transaction, so when
 * [ExposedTransactionRunner] wraps several calls in one `transaction {}` they
 * all commit (or roll back) together - the property the outbox pattern needs.
 */

private fun ResultRow.toWallet() = Wallet(
    id = this[Wallets.id],
    userId = this[Wallets.userId],
    balanceCents = this[Wallets.balanceCents],
    currency = this[Wallets.currency],
)

private fun ResultRow.toTxRecord() = TxRecord(
    id = this[Transactions.id],
    projectId = this[Transactions.projectId],
    fromWallet = this[Transactions.fromWallet],
    toWallet = this[Transactions.toWallet],
    amountCents = this[Transactions.amountCents],
    status = this[Transactions.status],
    idempotencyKey = this[Transactions.idempotencyKey],
    rideId = this[Transactions.rideId],
    providerRef = this[Transactions.providerRef],
    idempotencyArgsHash = this[Transactions.idempotencyArgsHash],
    kind = this[Transactions.kind],
    cardFunded = this[Transactions.cardFunded],
    createdAt = this[Transactions.createdAt],
)

class ExposedWalletRepository : WalletRepository {
    override fun getOrCreateByUser(projectId: UUID, userId: UUID): Wallet = transaction {
        scoped(projectId, userId).singleOrNull()?.toWallet()
            ?: run {
                try {
                    Wallets.insert {
                        it[Wallets.projectId] = projectId
                        it[Wallets.userId] = userId
                        it[balanceCents] = 0
                        it[currency] = "USD"
                        it[updatedAt] = Instant.now()
                    }
                } catch (e: ExposedSQLException) {
                    // 23503 is a foreign-key violation, which here means
                    // the user does not exist IN THIS PROJECT — the
                    // composite key added by migration 0080. That is the
                    // caller naming somebody else's user (or a user id
                    // that is simply wrong), so it must surface as their
                    // mistake rather than as an internal error.
                    if (e.sqlState == "23503") throw PaymentError.UnknownUser(userId)
                    // 23505 means a concurrent create won the race; ignore
                    // it and re-read the winning row below.
                    if (e.sqlState != "23505") throw e
                }
                scoped(projectId, userId).single().toWallet()
            }
    }

    override fun findByUser(projectId: UUID, userId: UUID): Wallet? = transaction {
        scoped(projectId, userId).singleOrNull()?.toWallet()
    }

    override fun findById(projectId: UUID, id: UUID): Wallet? = transaction {
        Wallets.selectAll()
            .where { (Wallets.projectId eq projectId) and (Wallets.id eq id) }
            .singleOrNull()?.toWallet()
    }

    // The project predicate is redundant while walletId comes from a
    // scoped lookup, and that is exactly why it is here: this is the
    // statement that moves money, and it should not depend on every
    // caller having been careful.
    override fun adjustBalance(projectId: UUID, walletId: UUID, deltaCents: Long) {
        transaction {
            Wallets.update({ (Wallets.projectId eq projectId) and (Wallets.id eq walletId) }) {
                it[balanceCents] = balanceCents + deltaCents
                it[updatedAt] = Instant.now()
            }
        }
    }

    private fun scoped(projectId: UUID, userId: UUID) =
        Wallets.selectAll().where { (Wallets.projectId eq projectId) and (Wallets.userId eq userId) }
}

class ExposedTransactionRepository : TransactionRepository {
    override fun findByIdempotencyKey(projectId: UUID, key: String): TxRecord? = transaction {
        Transactions.selectAll()
            .where { (Transactions.projectId eq projectId) and (Transactions.idempotencyKey eq key) }
            .singleOrNull()?.toTxRecord()
    }

    override fun findById(projectId: UUID, id: UUID): TxRecord? = transaction {
        Transactions.selectAll()
            .where { (Transactions.projectId eq projectId) and (Transactions.id eq id) }
            .singleOrNull()?.toTxRecord()
    }

    override fun findByProviderRef(projectId: UUID, providerRef: String): TxRecord? = transaction {
        // Served by the partial unique index idx_transactions_project_provider_ref
        // (migration 0090) — a point lookup, not a scan.
        Transactions.selectAll()
            .where { (Transactions.projectId eq projectId) and (Transactions.providerRef eq providerRef) }
            .singleOrNull()?.toTxRecord()
    }

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
        cardFunded: Boolean,
    ): TxRecord = transaction {
        try {
            val id = Transactions.insert {
                it[Transactions.projectId] = projectId
                it[Transactions.fromWallet] = fromWallet
                it[Transactions.toWallet] = toWallet
                it[Transactions.amountCents] = amountCents
                it[status] = TxStatus.PENDING
                it[Transactions.idempotencyKey] = idempotencyKey
                it[Transactions.rideId] = rideId
                it[Transactions.providerRef] = providerRef
                it[idempotencyArgsHash] = argsHash
                it[Transactions.kind] = kind
                it[Transactions.cardFunded] = cardFunded
                it[createdAt] = Instant.now()
            } get Transactions.id
            Transactions.selectAll().where { Transactions.id eq id }.single().toTxRecord()
        } catch (e: ExposedSQLException) {
            if (e.sqlState == "23505") throw DuplicateIdempotencyKey(idempotencyKey)
            throw e
        }
    }

    // Compare-and-set: each UPDATE carries the expected current status, so
    // only one of two racing writers flips the row and sees count 1. The
    // loser gets 0, skips the balance movement, and the money is applied
    // exactly once no matter how many webhook deliveries arrive together.
    override fun transitionSettled(projectId: UUID, id: UUID, settledAt: Instant): Int = transaction {
        Transactions.update({
            (Transactions.projectId eq projectId) and (Transactions.id eq id) and
                (Transactions.status eq TxStatus.PENDING)
        }) {
            it[status] = TxStatus.SETTLED
            it[Transactions.settledAt] = settledAt
        }
    }

    override fun transitionRefunded(projectId: UUID, id: UUID): Int = transaction {
        Transactions.update({
            (Transactions.projectId eq projectId) and (Transactions.id eq id) and
                (Transactions.status eq TxStatus.SETTLED)
        }) {
            it[status] = TxStatus.REFUNDED
        }
    }

    override fun transitionFailed(projectId: UUID, id: UUID, reason: String): Int = transaction {
        Transactions.update({
            (Transactions.projectId eq projectId) and (Transactions.id eq id) and
                (Transactions.status eq TxStatus.PENDING)
        }) {
            it[status] = TxStatus.FAILED
        }
    }.also {
        // The reason is logged, not stored: no column exists for it and the
        // provider_ref is what reconciliation actually needs.
        if (it > 0) LOG.warn("transaction {} marked failed: {}", id, reason)
    }

    override fun transitionCancelled(projectId: UUID, id: UUID, reason: String): Int = transaction {
        Transactions.update({
            (Transactions.projectId eq projectId) and (Transactions.id eq id) and
                (Transactions.status eq TxStatus.PENDING)
        }) {
            it[status] = TxStatus.CANCELLED
        }
    }.also {
        if (it > 0) LOG.warn("transaction {} cancelled: {}", id, reason)
    }

    override fun markFailed(projectId: UUID, id: UUID, reason: String) {
        transitionFailed(projectId, id, reason)
    }

    /**
     * `ORDER BY created_at ASC` so the oldest — the ones a customer has
     * been waiting on longest — are resolved first when the batch limit
     * bites.
     */
    override fun findStuckPending(cutoff: Instant, limit: Int): List<TxRecord> = transaction {
        Transactions
            .selectAll()
            .where {
                (Transactions.status eq TxStatus.PENDING) and
                    (Transactions.createdAt less cutoff)
            }
            .orderBy(Transactions.createdAt to SortOrder.ASC)
            .limit(limit)
            .map { it.toTxRecord() }
    }

    private companion object {
        private val LOG = org.slf4j.LoggerFactory.getLogger(ExposedTransactionRepository::class.java)
    }
}

/** Production [TransactionRunner]: one Exposed transaction per [run]. */
class ExposedTransactionRunner : TransactionRunner {
    override fun <T> run(block: () -> T): T = transaction { block() }
}
