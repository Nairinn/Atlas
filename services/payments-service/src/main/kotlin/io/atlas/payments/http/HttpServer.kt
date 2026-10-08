package io.atlas.payments.http

import io.atlas.payments.core.PaymentProvider
import io.atlas.payments.core.OutboxBackend
import io.atlas.payments.core.PaymentsMetrics
import io.atlas.payments.core.StripeWebhookHandler
import io.ktor.http.ContentType
import io.ktor.http.HttpHeaders
import io.ktor.http.HttpStatusCode
import io.ktor.server.application.call
import io.ktor.server.engine.embeddedServer
import io.ktor.server.netty.Netty
import io.ktor.server.netty.NettyApplicationEngine
import io.ktor.server.request.receiveText
import io.ktor.server.response.respond
import io.ktor.server.response.respondText
import io.ktor.server.routing.get
import io.ktor.server.routing.post
import io.ktor.server.routing.routing
import io.micrometer.core.instrument.MeterRegistry
import io.micrometer.prometheusmetrics.PrometheusConfig
import io.micrometer.prometheusmetrics.PrometheusMeterRegistry
import org.slf4j.LoggerFactory
import java.util.UUID

private val LOG = LoggerFactory.getLogger("io.atlas.payments.http.HttpServer")

/**
 * Single Ktor/Netty HTTP server serving both the Prometheus scrape endpoint and
 * provider webhooks. Consolidating them onto one port keeps the service to one
 * HTTP listener (gRPC is separate, on its own port).
 *
 *   GET  /metrics                       Prometheus exposition (Micrometer)
 *   GET  /healthz                       liveness ping
 *   POST /webhooks/stripe/{project_id}  Stripe events for one project
 *
 * The Stripe endpoint is per-project because each tenant registers its own
 * webhook endpoint in its own Stripe dashboard, so the signing secret — and
 * the credentials the events refer to — are per tenant. The project id in
 * the path selects the config row the signature is verified against.
 */
fun startHttpServer(
    port: Int,
    registry: PrometheusMeterRegistry,
    provider: PaymentProvider,
    metrics: PaymentsMetrics,
    webhookHandler: ((projectId: UUID, eventType: String, payload: String) -> StripeWebhookHandler.Result)? = null,
): NettyApplicationEngine =
    embeddedServer(Netty, port = port) {
        routing {
            get("/metrics") {
                call.respondText(registry.scrape(), ContentType.Text.Plain)
            }
            get("/healthz") {
                call.respondText("ok")
            }
            post("/webhooks/stripe/{project_id}") {
                val projectId = call.parameters["project_id"]
                    ?.let { runCatching { UUID.fromString(it) }.getOrNull() }
                if (projectId == null) {
                    call.respond(HttpStatusCode.BadRequest)
                    return@post
                }

                // Bounded before reading: the endpoint is unauthenticated by
                // nature — the signature covers the body, so the body must be
                // read before it can be checked. The cap stops an anonymous
                // caller from making the service allocate memory at will.
                val declared = call.request.headers[HttpHeaders.ContentLength]?.toLongOrNull()
                if (declared != null && declared > MAX_WEBHOOK_BYTES) {
                    metrics.webhookProcessed(BAD_SIGNATURE_FREE_OUTCOME)
                    call.respond(HttpStatusCode.PayloadTooLarge)
                    return@post
                }
                val body = call.receiveText()
                if (body.length > MAX_WEBHOOK_BYTES) {
                    call.respond(HttpStatusCode.PayloadTooLarge)
                    return@post
                }

                // Verify BEFORE doing anything with the payload, against
                // THIS project's webhook secret.
                val signature = call.request.headers["Stripe-Signature"]
                if (!provider.verifyWebhook(projectId, body, signature)) {
                    LOG.warn("rejected stripe webhook for project={}: bad signature", projectId)
                    metrics.webhookProcessed("bad_signature")
                    call.respond(HttpStatusCode.Unauthorized)
                    return@post
                }

                if (webhookHandler == null) {
                    // Verified but nobody is acting on it — an honest 202.
                    call.respond(HttpStatusCode.Accepted)
                    return@post
                }

                val eventType = call.request.headers["Stripe-Event-Type"]
                    ?: webhookEventTypeFromBody(body)
                val outcome = when (val result = webhookHandler(projectId, eventType, body)) {
                    is StripeWebhookHandler.Result.Applied -> {
                        LOG.info("stripe webhook project={} applied: {}", projectId, result.what)
                        "applied"
                    }
                    is StripeWebhookHandler.Result.Ignored -> {
                        LOG.debug("stripe webhook project={} ignored: {}", projectId, result.why)
                        "ignored"
                    }
                    is StripeWebhookHandler.Result.Malformed -> {
                        LOG.warn("stripe webhook project={} malformed: {}", projectId, result.why)
                        "malformed"
                    }
                }
                metrics.webhookProcessed(outcome)
                // All three outcomes are "delivered" from Stripe's point of
                // view. A 500 would mean retry-forever on an event that will
                // never parse better the second time.
                call.respond(HttpStatusCode.OK)
            }
        }
    }.start(wait = false)

/**
 * Largest webhook body accepted. Provider events run to a few kilobytes.
 */
private const val MAX_WEBHOOK_BYTES = 1_000_000

/** Oversized bodies are refused before any signature work: not a signature failure. */
private const val BAD_SIGNATURE_FREE_OUTCOME = "oversized"

/**
 * The event type from the body when the header is absent. Stripe does not
 * reliably send a type header, so the fallback reads `type` off the JSON
 * envelope — the one field every event has.
 */
private fun webhookEventTypeFromBody(body: String): String =
    Regex(""""type"\s*:\s*"([^"]+)"""").find(body)?.groupValues?.getOrNull(1) ?: "unknown"

/** Creates the Prometheus registry App.kt shares with the HTTP server and metrics. */
fun newPrometheusRegistry(): PrometheusMeterRegistry =
    PrometheusMeterRegistry(PrometheusConfig.DEFAULT)

/**
 * Register outbox depth as GAUGES fed by a supplier, so Micrometer polls at
 * scrape time and the value is never stale. Depth and age are the signals
 * that mean "stuck": the dispatched counter is a counter of SUCCESSES, and
 * a counter that stops increasing is indistinguishable from nothing to do.
 */
fun registerOutboxGauges(registry: MeterRegistry, backend: OutboxBackend) {
    registry.gauge("atlas_payments_outbox_pending_rows", backend) {
        it.pending().rows.toDouble()
    }
    registry.gauge("atlas_payments_outbox_oldest_age_seconds", backend) {
        it.pending().oldestAgeSeconds.toDouble()
    }
}

class MicrometerPaymentsMetrics(registry: MeterRegistry) : PaymentsMetrics {
    private val initiated = registry.counter("atlas_payments_transactions_initiated_total")
    private val reconciledSettled =
        registry.counter("atlas_payments_reconciled_total", "outcome", "settled")
    private val reconciledFailed =
        registry.counter("atlas_payments_reconciled_total", "outcome", "failed")
    // The one worth alerting on: money whose fate nobody knows.
    private val reconciledUnresolved =
        registry.counter("atlas_payments_reconciled_total", "outcome", "unresolved")
    private val settled = registry.counter("atlas_payments_transactions_settled_total")
    private val refunded = registry.counter("atlas_payments_transactions_refunded_total")
    private val cancelled = registry.counter("atlas_payments_transactions_cancelled_total")
    private val dispatched = registry.counter("atlas_payments_outbox_dispatched_total")
    private val depositOk = registry.counter("atlas_payments_deposits_total", "outcome", "settled")
    private val depositFail = registry.counter("atlas_payments_deposits_total", "outcome", "failed")
    private val webhook =
        registry.counter("atlas_payments_webhook_total", "outcome", "applied")
    private val webhookIgnored =
        registry.counter("atlas_payments_webhook_total", "outcome", "ignored")
    private val webhookMalformed =
        registry.counter("atlas_payments_webhook_total", "outcome", "malformed")
    private val webhookBadSignature =
        registry.counter("atlas_payments_webhook_total", "outcome", "bad_signature")

    override fun transactionInitiated() = initiated.increment()
    override fun transactionSettled() = settled.increment()
    override fun transactionRefunded() = refunded.increment()
    override fun transactionCancelled() = cancelled.increment()
    override fun reconciled(settled: Int, failed: Int, unresolved: Int) {
        if (settled > 0) reconciledSettled.increment(settled.toDouble())
        if (failed > 0) reconciledFailed.increment(failed.toDouble())
        // Always recorded, including zero, so the series exists before
        // anything goes wrong.
        reconciledUnresolved.increment(unresolved.toDouble())
    }

    override fun outboxDispatched(count: Int) = dispatched.increment(count.toDouble())
    override fun depositSettled() = depositOk.increment()
    override fun depositFailed() = depositFail.increment()

    override fun webhookProcessed(outcome: String) {
        when (outcome) {
            "applied" -> webhook.increment()
            "ignored" -> webhookIgnored.increment()
            "malformed" -> webhookMalformed.increment()
            "bad_signature" -> webhookBadSignature.increment()
            // Unknown outcomes (e.g. the oversized pre-check) are not
            // counted: they never reached the handler.
        }
    }
}
