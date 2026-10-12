package io.atlas.fare

import io.atlas.fare.config.EnvConfig
import io.atlas.fare.core.FareEventHandler
import io.atlas.common.DatabaseBootstrap
import io.atlas.fare.db.ExposedAuditLog
import io.atlas.fare.db.ExposedTransactionLookup
import io.atlas.fare.grpc.PaymentsGrpcCommands
import io.atlas.fare.http.MicrometerFareMetrics
import io.atlas.fare.http.newPrometheusRegistry
import io.atlas.fare.http.startHttpServer
import io.atlas.fare.kafka.FareEventConsumer
import org.slf4j.LoggerFactory
import java.util.concurrent.CountDownLatch

/**
 * Entry point for the fare-consumer.
 *
 * Wires the ports in `core` to their real adapters, starts the Kafka loop
 * and an HTTP server for /metrics and /healthz, then parks until a signal
 * arrives.
 */
private val LOG = LoggerFactory.getLogger("io.atlas.fare.App")

fun main() {
    val config = EnvConfig.fromEnv()
    LOG.info(
        "starting fare-consumer: topic={} group={} payments={} httpPort={}",
        config.fareTopic, config.consumerGroup, config.paymentsTarget, config.httpPort,
    )

    DatabaseBootstrap.connect(
        jdbcUrl = config.databaseUrl,
        username = config.databaseUser,
        password = config.databasePassword,
        poolName = "atlas-fare-consumer-pool",
    )

    val registry = newPrometheusRegistry()
    val payments = PaymentsGrpcCommands.build(config.paymentsTarget)
    val handler = FareEventHandler(
        payments = payments,
        lookup = ExposedTransactionLookup(),
        audit = ExposedAuditLog(),
        metrics = MicrometerFareMetrics(registry),
    )

    val consumer = FareEventConsumer.build(
        bootstrapServers = config.kafkaBrokers,
        groupId = config.consumerGroup,
        handler = handler,
        topic = config.fareTopic,
    )

    val http = startHttpServer(config.httpPort, registry)
    consumer.start()
    LOG.info("fare-consumer running")

    // Park the main thread; the consumer runs on its own non-daemon thread.
    val shutdown = CountDownLatch(1)
    Runtime.getRuntime().addShutdownHook(
        Thread {
            LOG.info("shutdown signal received, draining")
            consumer.close()
            payments.close()
            http.stop(1_000, 5_000)
            shutdown.countDown()
            LOG.info("fare-consumer stopped")
        },
    )
    shutdown.await()
}
