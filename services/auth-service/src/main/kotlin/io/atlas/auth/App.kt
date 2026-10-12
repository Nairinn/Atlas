package io.atlas.auth

import io.atlas.auth.cache.TokenValidationCache
import io.atlas.auth.config.EnvConfig
import io.atlas.auth.core.AuthService
import io.atlas.auth.core.LoggingEmailSender
import io.atlas.auth.crypto.BcryptPasswordHasher
import io.atlas.auth.crypto.Jose4jJwtSigner
import io.atlas.auth.crypto.SigningKey
import io.atlas.common.DatabaseBootstrap
import io.atlas.auth.db.ExposedSessionRepository
import io.atlas.auth.db.ExposedUserRepository
import io.atlas.auth.db.ExposedVerificationTokenRepository
import io.atlas.auth.grpc.AuthGrpcService
import io.atlas.common.HealthCheck
import io.atlas.auth.http.MicrometerAuthMetrics
import io.atlas.auth.http.newPrometheusRegistry
import io.atlas.auth.http.startHttpServer
import io.atlas.auth.kafka.AuthTokenProducer
import io.atlas.auth.kafka.TokenEventConsumer
import io.grpc.ServerBuilder
import org.slf4j.LoggerFactory
import java.time.Clock
import java.time.Duration
import java.util.concurrent.TimeUnit

/**
 * Phase 2B entry point. Reads config from env, opens the Postgres pool,
 * constructs the AuthService dependency graph, starts a gRPC server on
 * :50051 with grpc.health.v1.Health registered, fires up the Kafka producer
 * and the cache-invalidation consumer, and waits for the JVM to terminate.
 *
 * Graceful shutdown stops accepting new RPCs, flips health to NOT_SERVING,
 * drains in-flight requests, closes Kafka clients, and closes the DB pool.
 */
private val LOG = LoggerFactory.getLogger("io.atlas.auth.App")

fun main() {
    val config = EnvConfig.fromEnv()
    LOG.info(
        "starting auth-service: grpcPort={} httpPort={} dbUrl={} kafkaBrokers={}",
        config.grpcPort, config.httpPort, config.databaseUrl, config.kafkaBrokers,
    )

    DatabaseBootstrap.connect(
        jdbcUrl = config.databaseUrl,
        username = config.databaseUser,
        password = config.databasePassword,
        poolName = "atlas-auth-pool",
    )

    val users = ExposedUserRepository()
    val sessions = ExposedSessionRepository()
    val verificationTokens = ExposedVerificationTokenRepository()
    val hasher = BcryptPasswordHasher(cost = 12)
    val signer = Jose4jJwtSigner(
        active = SigningKey(config.jwtKeyId, config.jwtSecret),
        retired = config.jwtRetiredKeys.map { (id, secret) -> SigningKey(id, secret) },
    )
    if (config.jwtRetiredKeys.isNotEmpty()) {
        // Worth a line in the log: retired keys are meant to be temporary,
        // and one left in place indefinitely is a key that can still mint
        // nothing but can still verify everything.
        LOG.info(
            "JWT rotation in progress: active={} retired={}",
            config.jwtKeyId,
            config.jwtRetiredKeys.map { it.first },
        )
    }

    // Atlas sends no mail itself; this is the seam a provider plugs into.
    // The logging sender prints the reset token to the log, which is what
    // makes local development possible and what makes production
    // dangerous — so it has to be asked for by name.
    val emailSender = if (config.allowLoggingEmail) {
        LOG.warn(
            "using LoggingEmailSender: password reset tokens WILL be written " +
                "to the log. Never set ATLAS_ALLOW_LOGGING_EMAIL in production.",
        )
        LoggingEmailSender()
    } else {
        // Null, not a fake. The service still serves login and
        // registration; the two flows that need mail answer 412 naming
        // the missing configuration. Booting a fake sender by default
        // would instead accept reset requests and drop them, which a user
        // experiences as "the email never arrived" and an operator sees
        // as nothing at all.
        LOG.warn(
            "no email provider configured: password reset and email " +
                "verification will return FAILED_PRECONDITION. Set " +
                "ATLAS_ALLOW_LOGGING_EMAIL=true for development.",
        )
        null
    }

    val authService = AuthService(
        users = users,
        sessions = sessions,
        hasher = hasher,
        signer = signer,
        tokenLifetime = Duration.ofSeconds(config.tokenLifetimeSeconds),
        clock = Clock.systemUTC(),
        verificationTokens = verificationTokens,
        email = emailSender,
    )

    val registry = newPrometheusRegistry()
    val metrics = MicrometerAuthMetrics(registry)

    val cache = TokenValidationCache()
    val producer = AuthTokenProducer.build(config.kafkaBrokers, metrics)
    val consumer = TokenEventConsumer(
        bootstrapServers = config.kafkaBrokers,
        cache = cache,
    )

    val grpcService = AuthGrpcService(
        authService = authService,
        sessions = sessions,
        signer = signer,
        cache = cache,
        publisher = producer,
        metrics = metrics,
    )

    val health = HealthCheck()

    val server = ServerBuilder.forPort(config.grpcPort)
        .addService(grpcService)
        .addService(health.service)
        .build()

    val httpServer = startHttpServer(config.httpPort, registry)
    server.start()
    health.setServing()
    health.setServing("atlas.auth.AuthService")
    consumer.start()
    LOG.info(
        "started gRPC server on :{} and HTTP server on :{}",
        config.grpcPort, config.httpPort,
    )

    Runtime.getRuntime().addShutdownHook(Thread {
        LOG.info("shutdown signal received, draining")
        health.setNotServing()
        health.shutdown()
        try {
            server.shutdown().awaitTermination(15, TimeUnit.SECONDS)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
        consumer.close()
        producer.close()
        httpServer.stop(1_000, 5_000)
        LOG.info("auth-service stopped")
    })

    server.awaitTermination()
}
