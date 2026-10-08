package io.atlas.payments

import io.atlas.payments.config.EnvConfig
import io.atlas.payments.core.PaymentProviders
import io.atlas.payments.core.PaymentConfigSource
import io.atlas.payments.core.PerProjectPaymentProvider
import io.atlas.payments.core.ReconciliationSweep
import io.atlas.payments.core.RetryingPaymentProvider
import io.atlas.payments.core.PaymentsService
import io.atlas.payments.core.SettlementApplier
import io.atlas.payments.core.StaticPaymentConfigSource
import io.atlas.payments.core.StripeWebhookHandler
import io.atlas.payments.crypto.ConfigCipher
import io.atlas.payments.db.DatabaseBootstrap
import io.atlas.payments.db.ExposedOutboxBackend
import io.atlas.payments.db.ExposedOutboxStore
import io.atlas.payments.db.ExposedPaymentConfigSource
import io.atlas.payments.db.ExposedPayoutAccountSource
import io.atlas.payments.db.ExposedTransactionRepository
import io.atlas.payments.db.ExposedTransactionRunner
import io.atlas.payments.db.ExposedWalletRepository
import io.atlas.payments.grpc.HealthCheck
import io.atlas.payments.grpc.PaymentsGrpcService
import io.atlas.payments.http.MicrometerPaymentsMetrics
import io.atlas.payments.http.newPrometheusRegistry
import io.atlas.payments.http.registerOutboxGauges
import io.atlas.payments.http.startHttpServer
import io.atlas.payments.kafka.FareEventProducer
import io.atlas.payments.outbox.OutboxDispatcher
import io.atlas.payments.stripe.StripeOnboarding
import io.atlas.payments.stripe.StripePaymentProvider
import io.grpc.ServerBuilder
import org.slf4j.LoggerFactory
import java.time.Clock
import java.time.Duration
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * Phase 4 entry point. Reads config from env, opens the Postgres pool, builds
 * the PaymentsService graph, starts a gRPC server on :50053 (with
 * grpc.health.v1.Health), a Ktor HTTP server on :8053 (/metrics + webhooks),
 * and the background outbox dispatcher, then waits for the JVM to terminate.
 *
 * Graceful shutdown flips health to NOT_SERVING, stops the dispatcher, drains
 * in-flight RPCs, stops the HTTP server, and closes the Kafka producer.
 */
/** Delay before the first sweep, so a rolling deploy does not stampede. */
private const val RECONCILE_INITIAL_DELAY_SECONDS = 120L

/** How often to sweep. Stuck rows are not urgent, but they are not fine. */
private const val RECONCILE_INTERVAL_SECONDS = 300L

private val LOG = LoggerFactory.getLogger("io.atlas.payments.App")

fun main() {
    val config = EnvConfig.fromEnv()
    LOG.info(
        "starting payments-service: grpcPort={} httpPort={} dbUrl={} kafkaBrokers={} fareTopic={}",
        config.grpcPort, config.httpPort, config.databaseUrl, config.kafkaBrokers, config.fareTopic,
    )

    DatabaseBootstrap.connect(
        jdbcUrl = config.databaseUrl,
        username = config.databaseUser,
        password = config.databasePassword,
    )

    val registry = newPrometheusRegistry()
    val metrics = MicrometerPaymentsMetrics(registry)

    val wallets = ExposedWalletRepository()
    val transactions = ExposedTransactionRepository()
    val outboxStore = ExposedOutboxStore()
    val runner = ExposedTransactionRunner()

    // Per-project processor configuration. The encryption key is required
    // only when a project has actually connected Stripe: a deployment
    // where every project runs the fake provider must not be locked out
    // by a missing env var it would never use.
    val cipher = config.paymentConfigEncKey?.let { ConfigCipher(it) }
    val configSource: PaymentConfigSource = cipher?.let { ExposedPaymentConfigSource(it) }
        ?: StaticPaymentConfigSource(emptyMap())
    val payoutSource = ExposedPayoutAccountSource()

    // Connect onboarding: the Stripe adapter plus the persistence of the
    // account row it creates. Only when the encryption key is present, for
    // the same reason as the adapter below — onboarding without it would
    // create accounts at Stripe that Atlas could never charge through.
    val onboarding: StripeOnboarding? =
        if (config.paymentConfigEncKey != null) {
            StripeOnboarding(
                configs = configSource,
                accounts = payoutSource,
                apiBase = config.stripeApiBase,
            )
        } else {
            null
        }

    // The Stripe adapter, when any project could be configured for it.
    // The client-per-project cache inside means one instance serves every
    // tenant; STRIPE_API_BASE is a test/stripe-mock override. Built only
    // when the encryption key is present, since a config row cannot even
    // be decrypted without it.
    val stripeProvider: StripePaymentProvider? =
        if (config.paymentConfigEncKey != null) {
            StripePaymentProvider(
                configs = configSource,
                apiBase = config.stripeApiBase,
            )
        } else {
            null
        }

    // Provider selection per project: a project with a 'stripe' config
    // row talks to Stripe, everything else talks to the default named by
    // PAYMENT_PROVIDER. Wrapped so every provider call is bounded and the
    // safely-retryable ones are retried. See RetryingPaymentProvider for
    // why capture and refund deliberately are not.
    val provider = RetryingPaymentProvider(
        PerProjectPaymentProvider(
            default = PaymentProviders.fromName(config.paymentProvider),
            configs = configSource,
            stripe = stripeProvider,
        ),
    )
    LOG.info("payment provider: {} (default); stripe per-project where configured", provider.name)

    val payments = PaymentsService(
        wallets = wallets,
        transactions = transactions,
        outbox = outboxStore,
        runner = runner,
        provider = provider,
        fareTopic = config.fareTopic,
        payoutAccounts = payoutSource,
        onboardingStarter = onboarding?.let { o -> o::start },
        clock = Clock.systemUTC(),
        metrics = metrics,
    )

    val publisher = FareEventProducer.build(config.kafkaBrokers)
    val outboxBackend = ExposedOutboxBackend()
    // Scraped, not pushed: a stuck outbox is money not moving, and the
    // gauge is what makes that visible without anyone querying the table.
    registerOutboxGauges(registry, outboxBackend)

    val dispatcher = OutboxDispatcher(
        backend = outboxBackend,
        publisher = publisher,
        pollInterval = Duration.ofSeconds(config.outboxPollSeconds),
        batchSize = config.outboxBatchSize,
        metrics = metrics,
    )

    val grpcService = PaymentsGrpcService(payments, dispatcher)
    val health = HealthCheck()

    val server = ServerBuilder.forPort(config.grpcPort)
        .addService(grpcService)
        .addService(health.service)
        .build()

    // One settlement implementation for the RPC path, the webhook, and
    // the reconciliation sweep — the drift item the audit called out.
    val settlementApplier = SettlementApplier(
        wallets = wallets,
        transactions = transactions,
        runner = runner,
        outbox = outboxStore,
        fareTopic = config.fareTopic,
        clock = Clock.systemUTC(),
    )

    val httpServer = startHttpServer(config.httpPort, registry, provider, metrics) { projectId, eventType, body ->
        StripeWebhookHandler(
            projectId = projectId,
            transactions = transactions,
            applier = settlementApplier,
            payoutAccounts = payoutSource,
        ).handle(eventType, body)
    }
    server.start()
    health.setServing()
    health.setServing("atlas.payments.PaymentsService")
    dispatcher.start()

    // Resolves transactions left PENDING by a crash or a lost provider
    // response. Runs on a timer rather than on demand because the rows it
    // fixes are, by definition, ones nobody is watching.
    val reconciliation = ReconciliationSweep(
        transactions = transactions,
        applier = settlementApplier,
        provider = provider,
        metrics = metrics,
    )
    val reconciler = Executors.newSingleThreadScheduledExecutor { r ->
        Thread(r, "reconciliation").apply { isDaemon = true }
    }
    reconciler.scheduleWithFixedDelay(
        {
            try {
                val outcome = reconciliation.runOnce()
                if (outcome.total > 0) {
                    LOG.info(
                        "reconciliation: settled={} failed={} cancelled={} unresolved={}",
                        outcome.settled, outcome.failed, outcome.cancelled, outcome.unresolved,
                    )
                }
            } catch (e: Exception) {
                // A failed pass must never kill the schedule: the rows are
                // still there and the next tick retries them.
                LOG.warn("reconciliation pass failed; will retry", e)
            }
        },
        // Not at startup: during a rolling deploy several instances would
        // sweep at once, and the first minutes after a restart are when
        // in-flight transactions look most like stuck ones.
        RECONCILE_INITIAL_DELAY_SECONDS,
        RECONCILE_INTERVAL_SECONDS,
        TimeUnit.SECONDS,
    )

    LOG.info("started gRPC server on :{} and HTTP server on :{}", config.grpcPort, config.httpPort)

    Runtime.getRuntime().addShutdownHook(Thread {
        LOG.info("shutdown signal received, draining")
        health.setNotServing()
        health.shutdown()
        dispatcher.close()
        reconciler.shutdownNow()
        try {
            server.shutdown().awaitTermination(15, TimeUnit.SECONDS)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
        httpServer.stop(1_000, 5_000)
        publisher.close()
        LOG.info("payments-service stopped")
    })

    server.awaitTermination()
}
