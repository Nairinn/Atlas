package io.atlas.auth.kafka

import atlas.events.AuthTokenEvent
import io.atlas.auth.cache.TokenValidationCache
import org.apache.kafka.clients.consumer.ConsumerConfig
import org.apache.kafka.clients.consumer.KafkaConsumer
import org.apache.kafka.common.serialization.ByteArrayDeserializer
import org.apache.kafka.common.serialization.StringDeserializer
import org.slf4j.LoggerFactory
import java.time.Duration
import java.util.Properties
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * Subscribes to `atlas.auth.tokens` and evicts entries from the local
 * [TokenValidationCache] when REVOKED events arrive. Each auth-service
 * instance uses a unique consumer group so every instance receives every
 * event (fanout broadcast pattern), not the queue semantics of a shared
 * group.
 *
 * Lifecycle: [start] launches a single poller thread; [close] sets the
 * shutdown flag and waits for the poller to drain.
 */
class TokenEventConsumer(
    private val bootstrapServers: String,
    private val cache: TokenValidationCache,
    private val instanceId: String = UUID.randomUUID().toString(),
    private val topic: String = AuthTokenProducer.TOPIC,
) : AutoCloseable {

    private val running = AtomicBoolean(false)
    private var workerThread: Thread? = null

    fun start() {
        if (!running.compareAndSet(false, true)) return
        workerThread = thread(name = "auth-token-consumer", isDaemon = true) { runLoop() }
    }

    override fun close() {
        if (!running.compareAndSet(true, false)) return
        workerThread?.join(Duration.ofSeconds(5).toMillis())
    }

    private fun runLoop() {
        val props = Properties().apply {
            put(ConsumerConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrapServers)
            put(ConsumerConfig.GROUP_ID_CONFIG, "auth-service-cache-invalidation-$instanceId")
            put(ConsumerConfig.KEY_DESERIALIZER_CLASS_CONFIG, StringDeserializer::class.java.name)
            put(ConsumerConfig.VALUE_DESERIALIZER_CLASS_CONFIG, ByteArrayDeserializer::class.java.name)
            // Each instance starts at the latest offset: revocation events that
            // happened before this instance booted cannot retroactively
            // invalidate a cache that did not exist.
            put(ConsumerConfig.AUTO_OFFSET_RESET_CONFIG, "latest")
            put(ConsumerConfig.ENABLE_AUTO_COMMIT_CONFIG, true)
        }
        try {
            KafkaConsumer<String, ByteArray>(props).use { consumer ->
                consumer.subscribe(listOf(topic))
                LOG.info("subscribed to {} group=auth-service-cache-invalidation-{}", topic, instanceId)
                while (running.get()) {
                    val records = consumer.poll(Duration.ofMillis(500))
                    for (record in records) {
                        handleRecord(record.value())
                    }
                }
            }
        } catch (e: Exception) {
            // Don't crash the service if Kafka is unreachable; log and exit
            // the loop. Cache will fall back to 30s TTL invalidation.
            LOG.warn("token event consumer terminating: {}", e.message, e)
        }
    }

    private fun handleRecord(bytes: ByteArray) {
        val event = try {
            AuthTokenEvent.parseFrom(bytes)
        } catch (e: Exception) {
            LOG.warn("skipped malformed AuthTokenEvent: {}", e.message)
            return
        }
        if (event.eventType == AuthTokenEvent.EventType.REVOKED) {
            if (event.tokenHash.isBlank()) {
                // The user-wide form (password reset): no single hash to
                // name, so the peer drops every cached validation for the
                // user. Without this, only the instance that handled the
                // reset evicts, and every other replica keeps validating
                // pre-reset tokens for the rest of the 30s TTL.
                val userId = try {
                    java.util.UUID.fromString(event.userId)
                } catch (e: IllegalArgumentException) {
                    LOG.warn("skipped user-wide revocation with bad user_id {}", event.userId)
                    return
                }
                cache.evictUser(userId)
            } else {
                cache.evictByTokenHash(event.tokenHash)
            }
        }
    }

    companion object {
        private val LOG = LoggerFactory.getLogger(TokenEventConsumer::class.java)
    }
}
