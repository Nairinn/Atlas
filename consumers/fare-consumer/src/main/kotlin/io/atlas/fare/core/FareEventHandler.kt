package io.atlas.fare.core

import atlas.events.FareEvent
import org.slf4j.LoggerFactory
import java.security.MessageDigest
import java.util.UUID

/**
 * Decides what each fare event means and carries it out.
 *
 * # The reaction table
 *
 * | Event                | Action                          |
 * |----------------------|---------------------------------|
 * | RIDE_ACCEPTED        | audit only                      |
 * | RIDE_COMPLETED       | SettleTransaction               |
 * | RIDE_CANCELLED       | RefundTransaction               |
 * | TRANSACTION_SETTLED  | audit only                      |
 * | TRANSACTION_REFUNDED | audit only                      |
 * | TRANSACTION_CANCELLED | audit only                     |
 *
 * The TRANSACTION_* rows are the important ones. Payments publishes
 * them itself from inside settle/refund/cancel, onto the same topic
 * this consumer reads. Treating them as instructions would mean settling
 * in response to having settled — an infinite loop through Kafka. They
 * are acknowledgements, so they are only ever recorded.
 *
 * RIDE_ACCEPTED is likewise payments' own announcement that it created a
 * transaction; the transaction already exists by then.
 *
 * # Ordering: act first, then audit
 *
 * A replayed event must not skip its action. If the audit row were
 * written first, a crash in between would leave a log entry claiming the
 * event was handled while the settlement never happened, and the replay
 * would dedup itself into doing nothing. Acting first is safe in the
 * other direction because the money operations are idempotent upstream:
 * settling an already-settled transaction returns success without moving
 * anything.
 */
class FareEventHandler(
    private val payments: PaymentsCommands,
    private val lookup: TransactionLookup,
    private val audit: AuditLog,
    private val metrics: FareMetrics = FareMetrics.NOOP,
) {

    /** Whether the Kafka offset may advance past this event. */
    enum class Outcome { COMMIT, RETRY }

    fun handle(event: FareEvent): Outcome {
        val rideId = parseUuidOrNull(event.rideId)
        val eventTransactionId = parseUuidOrNull(event.transactionId)

        // An event with no project cannot be acted on: settle and refund
        // are scoped, so there is no tenant to scope them to. Committing
        // rather than retrying is right — a replay carries the same
        // missing field, so retrying would wedge the partition on one bad
        // record and stop every good one behind it.
        val projectId = parseUuidOrNull(event.projectId)
        if (projectId == null) {
            LOG.warn(
                "fare event has no usable project_id (ride_id='{}'); auditing only",
                event.rideId,
            )
            metrics.unresolved()
            return auditAndCommit(event, eventTransactionId, rideId)
        }

        if (requiresAction(event.eventType)) {
            val target = eventTransactionId ?: rideId?.let { lookup.findByRideId(projectId, it) }
            if (target == null) {
                // Nothing to act on. Either the event named neither a
                // transaction nor a ride, or the ride has no transaction —
                // a ride completing that was never paid for. Both are the
                // application's business, not a fault here, so record and
                // move on rather than retrying forever.
                LOG.warn(
                    "no transaction for {} event (ride_id='{}', transaction_id='{}'); auditing only",
                    event.eventType, event.rideId, event.transactionId,
                )
                metrics.unresolved()
                return auditAndCommit(event, null, rideId)
            }

            when (val result = act(projectId, event.eventType, target)) {
                is CommandResult.Applied ->
                    if (event.eventType == FareEvent.EventType.RIDE_COMPLETED) {
                        metrics.settled()
                    } else {
                        metrics.refunded()
                    }

                is CommandResult.Rejected ->
                    // Rare now that payments cancels PENDING rows on
                    // RIDE_CANCELLED: a rejection means a state the
                    // command does not cover. Record it and let the
                    // offset advance — retrying cannot help.
                {
                    LOG.info(
                        "payments rejected {} for transaction {}: {}",
                        event.eventType, target, result.reason,
                    )
                    metrics.rejected()
                }

                is CommandResult.Unavailable -> {
                    // Hold the offset. Redelivery is the recovery path.
                    LOG.warn(
                        "payments unavailable for {} on transaction {}: {}; will retry",
                        event.eventType, target, result.reason,
                    )
                    metrics.retried()
                    return Outcome.RETRY
                }
            }
            return auditAndCommit(event, target, rideId)
        }

        return auditAndCommit(event, eventTransactionId, rideId)
    }

    private fun act(
        projectId: UUID,
        type: FareEvent.EventType,
        transactionId: UUID,
    ): CommandResult =
        when (type) {
            FareEvent.EventType.RIDE_COMPLETED -> payments.settle(projectId, transactionId)
            FareEvent.EventType.RIDE_CANCELLED -> payments.refund(projectId, transactionId)
            else -> CommandResult.Applied
        }

    private fun auditAndCommit(event: FareEvent, transactionId: UUID?, rideId: UUID?): Outcome {
        val recorded = audit.record(
            AuditEntry(
                // Nil rather than a guess when the event had no project.
                // The row still gets written — an event we could not act
                // on is exactly the kind worth having in an audit log —
                // and the sentinel makes it findable.
                projectId = parseUuidOrNull(event.projectId) ?: UNKNOWN_PROJECT,
                eventKey = eventKey(event),
                transactionId = transactionId,
                rideId = rideId,
                eventType = event.eventType,
                payloadJson = payloadJson(event),
            ),
        )
        if (!recorded) {
            LOG.debug("duplicate fare event ignored: {}", event.eventType)
            metrics.duplicate()
        }
        return Outcome.COMMIT
    }

    companion object {
        private val LOG = LoggerFactory.getLogger(FareEventHandler::class.java)

        /**
         * Stand-in project for an audit row whose event carried no usable
         * project_id. The row is still worth writing — an event nobody
         * could act on is precisely what an audit log is for — and the
         * nil UUID makes those rows findable with one query rather than
         * hiding them among real tenants.
         */
        private val UNKNOWN_PROJECT: UUID = UUID(0L, 0L)

        /**
         * Events that move money. Everything else is an acknowledgement
         * of something that already happened.
         */
        fun requiresAction(type: FareEvent.EventType): Boolean = when (type) {
            FareEvent.EventType.RIDE_COMPLETED,
            FareEvent.EventType.RIDE_CANCELLED,
            -> true

            FareEvent.EventType.RIDE_ACCEPTED,
            FareEvent.EventType.TRANSACTION_SETTLED,
            FareEvent.EventType.TRANSACTION_REFUNDED,
            FareEvent.EventType.TRANSACTION_CANCELLED,
            FareEvent.EventType.UNKNOWN,
            FareEvent.EventType.UNRECOGNIZED,
            -> false
        }

        /**
         * Stable identity for one logical event, used to dedup the audit
         * log across redeliveries.
         *
         * Includes occurred_at so two genuinely distinct events of the
         * same type on the same ride — a retry of a cancelled ride, say —
         * are not collapsed into one row.
         */
        fun eventKey(event: FareEvent): String {
            val material =
                "${event.rideId}|${event.transactionId}|${event.eventType.number}|${event.occurredAt}"
            val digest = MessageDigest.getInstance("SHA-256").digest(material.toByteArray())
            return digest.joinToString("") { "%02x".format(it) }
        }

        fun payloadJson(event: FareEvent): String = buildString {
            append("{")
            append("\"ride_id\":\"").append(escape(event.rideId)).append("\",")
            append("\"transaction_id\":\"").append(escape(event.transactionId)).append("\",")
            append("\"event_type\":\"").append(event.eventType.name).append("\",")
            append("\"amount_cents\":").append(event.amountCents).append(",")
            append("\"occurred_at\":").append(event.occurredAt)
            append("}")
        }

        // ride_id and transaction_id are UUID strings in practice, but they
        // arrive from Kafka and this builds JSON by hand, so quotes and
        // backslashes get escaped rather than trusted.
        private fun escape(s: String): String =
            s.replace("\\", "\\\\").replace("\"", "\\\"")

        private fun parseUuidOrNull(value: String): UUID? =
            if (value.isBlank()) null
            else try {
                UUID.fromString(value)
            } catch (e: IllegalArgumentException) {
                null
            }
    }
}
