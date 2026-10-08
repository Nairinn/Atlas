package io.atlas.auth.kafka

import atlas.events.AuthTokenEvent
import io.atlas.auth.core.TokenClaims
import org.apache.kafka.clients.producer.KafkaProducer
import org.apache.kafka.clients.producer.Producer
import org.apache.kafka.clients.producer.ProducerConfig
import org.apache.kafka.clients.producer.ProducerRecord
import org.apache.kafka.common.serialization.ByteArraySerializer
import org.apache.kafka.common.serialization.StringSerializer
import io.atlas.auth.core.AuthMetrics
import org.slf4j.LoggerFactory
import java.time.Clock
import java.util.Properties
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.TimeUnit

/**
 * Publishes [AuthTokenEvent] protobufs to `atlas.auth.tokens` so other
 * services (gateway, observability, consumers) can react to token lifecycle
 * changes. Topic payload format: protobuf, per Atlas locked decisions.
 *
 * Key is the `user_id` string so all events for a single user partition to
 * the same broker, preserving per-user ordering.
 */
interface AuthTokenEventPublisher {
    fun publishIssued(claims: TokenClaims, rawToken: String)
    fun publishRevoked(claims: TokenClaims, rawToken: String)

    /**
     * Every session for [userId] is gone (password reset). Peers cannot
     * know the tokens' hashes, so the event is user-scoped; the consumer
     * evicts the whole user.
     */
    fun publishUserRevoked(userId: java.util.UUID)
    fun close()
}

/**
 * # Why publishing happens on a separate thread
 *
 * `KafkaProducer.send()` is asynchronous in its delivery callback but NOT in
 * its call: it blocks for up to `max.block.ms` while it fetches topic
 * metadata, and that default is 60 seconds. With the broker unreachable, a
 * direct `send()` on the request path meant `Authenticate` hung for a minute
 * and the caller timed out — a Kafka outage took login down with it. The old
 * comment on the callback claimed this path did not block the request; it did.
 *
 * So sends are handed to a single-threaded executor with a small bounded
 * queue, and the request path never touches Kafka. When the queue fills — the
 * broker is down and events are piling up — new events are dropped and
 * counted rather than blocking a caller or growing without limit. Dropping is
 * the right trade here: Postgres is the state of record for tokens, and this
 * topic only drives cache invalidation, which the 30s cache TTL recovers on
 * its own.
 *
 * `max.block.ms` is also lowered so a wedged producer cannot hold the
 * executor thread for a minute.
 */
class AuthTokenProducer(
    private val producer: Producer<String, ByteArray>,
    private val topic: String = TOPIC,
    private val clock: Clock = Clock.systemUTC(),
    private val metrics: AuthMetrics = AuthMetrics.NOOP,
) : AuthTokenEventPublisher {

    // One thread: per-user ordering is already guaranteed by the partition
    // key, and a single thread keeps event order as observed by this
    // instance. Queue of 1000 is roughly a few seconds of issuance at
    // realistic rates — enough to ride out a broker blip, small enough that
    // a sustained outage sheds load instead of hoarding heap.
    private val dispatcher = ThreadPoolExecutor(
        1, 1, 0L, TimeUnit.MILLISECONDS,
        ArrayBlockingQueue(1000),
        { r -> Thread(r, "auth-token-producer").apply { isDaemon = true } },
        // Discard the newest rather than block the submitting thread, which
        // is the whole point of this indirection.
        ThreadPoolExecutor.DiscardPolicy(),
    )

    override fun publishIssued(claims: TokenClaims, rawToken: String) {
        publish(claims, rawToken, AuthTokenEvent.EventType.ISSUED)
    }

    override fun publishRevoked(claims: TokenClaims, rawToken: String) {
        publish(claims, rawToken, AuthTokenEvent.EventType.REVOKED)
    }

    override fun publishUserRevoked(userId: java.util.UUID) {
        // An empty token_hash is the user-wide marker: the consumer sees
        // no hash to evict by and evicts the user instead.
        val event = AuthTokenEvent.newBuilder()
            .setUserId(userId.toString())
            .setEventType(AuthTokenEvent.EventType.REVOKED)
            .setOccurredAt(clock.instant().epochSecond)
            .build()
        val record = ProducerRecord(topic, userId.toString(), event.toByteArray())
        try {
            dispatcher.execute { producer.send(record) { _, exception ->
                if (exception != null) {
                    LOG.warn("failed to publish user-wide revocation userId={}", userId, exception)
                    metrics.tokenEventPublishFailed("delivery")
                }
            } }
        } catch (e: Exception) {
            metrics.tokenEventPublishFailed("dispatch")
        }
    }

    private fun publish(claims: TokenClaims, rawToken: String, type: AuthTokenEvent.EventType) {
        val event = AuthTokenEvent.newBuilder()
            .setUserId(claims.userId.toString())
            .setTokenHash(sha256Hex(rawToken))
            .setSessionId(claims.sessionId.toString())
            .setEventType(type)
            .setOccurredAt(clock.instant().epochSecond)
            .build()
        val record = ProducerRecord(topic, claims.userId.toString(), event.toByteArray())

        val accepted = try {
            dispatcher.execute {
                try {
                    producer.send(record) { _, exception ->
                        if (exception != null) {
                            // Token state of record is Postgres; this topic is
                            // a fanout channel for cache invalidation. A failed
                            // publish leaves the system degraded but correct —
                            // invalidations are missed and the 30s TTL recovers.
                            LOG.warn(
                                "failed to publish AuthTokenEvent type={} userId={}",
                                type, claims.userId, exception,
                            )
                            metrics.tokenEventPublishFailed("delivery")
                        }
                    }
                } catch (e: Exception) {
                    // send() itself throws on metadata timeout or a closed
                    // producer. On the executor thread this harms nothing.
                    LOG.warn("AuthTokenEvent send failed type={} userId={}", type, claims.userId, e)
                    metrics.tokenEventPublishFailed("send")
                }
            }
            true
        } catch (e: RejectedExecutionException) {
            // Only reachable after close(); DiscardPolicy handles a full queue
            // silently, which is why the drop counter below is checked
            // separately.
            false
        }

        if (!accepted) {
            metrics.tokenEventPublishFailed("rejected")
        } else if (dispatcher.queue.remainingCapacity() == 0) {
            // The queue is saturated, so this or a subsequent event is being
            // discarded. Counting here is approximate by design: the exact
            // discard happens inside DiscardPolicy, which has no hook.
            metrics.tokenEventPublishFailed("queue_full")
        }
    }

    override fun close() {
        // Stop accepting, drain what is queued, then close the producer.
        // Closing the producer first would fail every in-flight send.
        dispatcher.shutdown()
        if (!dispatcher.awaitTermination(5, TimeUnit.SECONDS)) {
            LOG.warn("token event dispatcher did not drain in 5s; {} event(s) dropped",
                dispatcher.queue.size)
            dispatcher.shutdownNow()
        }
        producer.close()
    }

    companion object {
        const val TOPIC = "atlas.auth.tokens"
        private val LOG = LoggerFactory.getLogger(AuthTokenProducer::class.java)

        fun build(
            bootstrapServers: String,
            metrics: AuthMetrics = AuthMetrics.NOOP,
        ): AuthTokenProducer {
            val props = Properties().apply {
                put(ProducerConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrapServers)
                put(ProducerConfig.KEY_SERIALIZER_CLASS_CONFIG, StringSerializer::class.java.name)
                put(ProducerConfig.VALUE_SERIALIZER_CLASS_CONFIG, ByteArraySerializer::class.java.name)
                put(ProducerConfig.ACKS_CONFIG, "all")
                put(ProducerConfig.ENABLE_IDEMPOTENCE_CONFIG, true)
                put(ProducerConfig.CLIENT_ID_CONFIG, "auth-service")
                put(ProducerConfig.LINGER_MS_CONFIG, 5)
                // Default is 60s. Even on the executor thread that is far
                // too long to sit on an unreachable broker.
                put(ProducerConfig.MAX_BLOCK_MS_CONFIG, 5_000)
            }
            return AuthTokenProducer(KafkaProducer(props), metrics = metrics)
        }
    }
}

/**
 * Stub publisher used by unit tests (and by App.kt if Kafka is intentionally
 * disabled for local-only experimentation). Records calls in-memory so tests
 * can assert on them.
 */
class RecordingAuthTokenPublisher : AuthTokenEventPublisher {
    data class Event(val type: AuthTokenEvent.EventType, val claims: TokenClaims, val rawToken: String)

    private val _events = mutableListOf<Event>()
    /** User ids whose whole session set was revoked (password reset). */
    val userRevocations = mutableListOf<java.util.UUID>()

    val events: List<Event> get() = synchronized(_events) { _events.toList() }

    override fun publishIssued(claims: TokenClaims, rawToken: String) {
        synchronized(_events) { _events += Event(AuthTokenEvent.EventType.ISSUED, claims, rawToken) }
    }

    override fun publishRevoked(claims: TokenClaims, rawToken: String) {
        synchronized(_events) { _events += Event(AuthTokenEvent.EventType.REVOKED, claims, rawToken) }
    }

    override fun publishUserRevoked(userId: java.util.UUID) {
        synchronized(_events) { userRevocations += userId }
    }

    override fun close() {}
}
