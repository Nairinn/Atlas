package io.atlas.payments.core

import java.util.UUID

/**
 * Transport-agnostic domain models. The Exposed and in-memory repositories both
 * map their rows to these; the service logic never sees a ResultRow or a proto.
 */

data class Wallet(
    val id: UUID,
    val userId: UUID,
    val balanceCents: Long,
    val currency: String,
)

data class TxRecord(
    val id: UUID,
    /**
     * Needed by anything that acts on a record it did not fetch by
     * project — reconciliation sweeps every tenant's stuck rows in one
     * pass, then writes each back scoped to its own.
     */
    val projectId: UUID,
    val fromWallet: UUID?,
    val toWallet: UUID?,
    val amountCents: Long,
    val status: String,
    val idempotencyKey: String,
    val rideId: UUID?,
    val providerRef: String?,
    val idempotencyArgsHash: String?,
    /** One of [TxKind]. Defaulted so existing construction sites are unchanged. */
    val kind: String = TxKind.TRANSFER,
    /**
     * True when the charge went to the processor with a destination
     * account (a Connect fare): the payer's card pays, not their wallet.
     * Fixed at initiation; settle and the webhook both read it.
     */
    val cardFunded: Boolean = false,
    /** When the row was created; the sweep's hold-expiry clock. */
    val createdAt: java.time.Instant? = null,
)

/** A pending outbox row claimed by the dispatcher for publishing. */
data class OutboxRow(
    val id: UUID,
    val transactionId: UUID?,
    val topic: String,
    val payload: ByteArray,
) {
    // ByteArray needs structural equals/hashCode for test assertions.
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is OutboxRow) return false
        return id == other.id &&
            transactionId == other.transactionId &&
            topic == other.topic &&
            payload.contentEquals(other.payload)
    }

    override fun hashCode(): Int {
        var result = id.hashCode()
        result = 31 * result + (transactionId?.hashCode() ?: 0)
        result = 31 * result + topic.hashCode()
        result = 31 * result + payload.contentHashCode()
        return result
    }
}

// Transaction status constants (mirrors the CHECK constraint in 0030_payments.sql).
/**
 * How money entered or left a transaction.
 *
 * WITHDRAWAL is declared but not yet implemented — payouts are the mirror
 * of deposits and need the same provider handling. It is here so the
 * status vocabulary is complete in one place rather than growing later.
 */
object TxKind {
    const val TRANSFER = "transfer"
    const val DEPOSIT = "deposit"
    const val WITHDRAWAL = "withdrawal"
}

object TxStatus {
    const val PENDING = "pending"
    const val SETTLED = "settled"
    const val FAILED = "failed"
    const val REFUNDED = "refunded"

    /** An authorization voided before capture; no money moved. */
    const val CANCELLED = "cancelled"
}
