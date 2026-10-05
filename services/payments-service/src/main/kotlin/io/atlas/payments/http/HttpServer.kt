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
 *   GET  /metrics                          Prometheus exposition (Micrometer)
 *   GET  /healthz                          liveness ping
 *   POST /webhooks/stripe/{project_id}     Stripe events for one project
 *   POST /webhooks/{provider}              legacy single-tenant path; kept
 *                                          only for the fake provider, which
 *                                          has no project scoping
 *
 * The Stripe endpoint is per-project because each tenant registers its own
 * webhook endpoint in its own Stripe dashboard, so the signing secret —
 * and the credentials the events refer to — are per tenant. The project id
 * in the path selects the config row the signature is verified against.
 */
fun startHttpServer(
    port: Int,
    registry: PrometheusMeterRegistry,
    provider: PaymentProvider,
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

                // Bounded before reading, same reasoning as below: this
                // endpoint is unauthenticated by nature.
                val declared = call.request.headers[HttpHeaders.ContentLength]?.toLongOrNull()
                if (declared != null && declared > MAX_WEBHOOK_BYTES) {
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
                    call.respond(HttpStatusCode.Unauthorized)
                    return@post
                }

                if (webhookHandler == null) {
                    // Verified but nobody is acting on it yet — an honest
                    // 202 rather than a lie of a 200.
                    call.respond(HttpStatusCode.Accepted)
                    return@post
                }

                val eventType = call.request.headers["Stripe-Event-Type"]
                    ?: webhookEventTypeFromBody(body)
                when (val result = webhookHandler(projectId, eventType, body)) {
                    is StripeWebhookHandler.Result.Applied ->
                        LOG.info("stripe webhook project={} applied: {}", projectId, result.what)
                    is StripeWebhookHandler.Result.Ignored ->
                        LOG.debug("stripe webhook project={} ignored: {}", projectId, result.why)
                    is StripeWebhookHandler.Result.Malformed ->
                        LOG.warn("stripe webhook project={} malformed: {}", projectId, result.why)
                }
                // All three outcomes are "delivered" from Stripe's point of
                // view. A 500 would mean retry-forever on an event that
                // will never parse better the second time.
                call.respond(HttpStatusCode.OK)
            }
            post("/webhooks/{provider}") {
                val source = call.parameters["provider"] ?: "unknown"

                // Bounded before reading. This endpoint is unauthenticated
                // by nature — the signature is checked after the body is
                // in hand, because the signature covers the body — so an
                // unbounded read here is a way for anyone on the internet
                // to make the service allocate as much memory as they
                // like. Provider events are a few kilobytes; 1MB is
                // generous.
                val declared = call.request.headers[HttpHeaders.ContentLength]?.toLongOrNull()
                if (declared != null && declared > MAX_WEBHOOK_BYTES) {
                    LOG.warn("rejected oversized webhook from provider={} bytes={}", source, declared)
                    call.respond(HttpStatusCode.PayloadTooLarge)
                    return@post
                }
                val body = call.receiveText()
                if (body.length > MAX_WEBHOOK_BYTES) {
                    // A chunked request declares no length, so the cap is
                    // enforced again on what actually arrived.
                    LOG.warn("rejected oversized webhook from provider={}", source)
                    call.respond(HttpStatusCode.PayloadTooLarge)
                    return@post
                }

                // Verify BEFORE doing anything with the payload. This
                // endpoint is an unauthenticated write path from the public
                // internet into a payments system; the only thing making it
                // safe is that the provider signed the request.
                //
                // The check runs even though FakePaymentProvider accepts
                // everything, so the call site exists and cannot be
                // forgotten when a real provider is wired in. Verification
                // is provider-specific, which is why it lives on the
                // PaymentProvider interface rather than here.
                val signature = call.request.headers["Stripe-Signature"]
                    ?: call.request.headers["X-Webhook-Signature"]
                if (!provider.verifyWebhook(projectIdFromPath(), body, signature)) {
                    LOG.warn("rejected webhook from provider={}: bad signature", source)
                    call.respond(HttpStatusCode.Unauthorized)
                    return@post
                }

                // Acknowledged and logged. Reconciling the referenced charge
                // against payments.transactions is the next step and needs a
                // real provider's event schema to be worth writing.
                LOG.info("accepted webhook from provider={} bytes={}", source, body.length)
                call.respond(HttpStatusCode.OK)
            }
        }
    }.start(wait = false)

/**
 * Largest webhook body accepted. Provider events run to a few kilobytes.
 */
private const val MAX_WEBHOOK_BYTES = 1_000_000

/**
 * The event type from the body when the header is absent. Stripe does not
 * reliably send a type header, so the fallback reads `type` off the JSON
 * envelope — the one field every event has.
 */
private fun webhookEventTypeFromBody(body: String): String =
    Regex(""""type"\s*:\s*"([^"]+)"""").find(body)?.groupValues?.getOrNull(1) ?: "unknown"

/**
 * The legacy single-tenant webhook path has no project in the URL, so it
 * verifies against... nothing, which is exactly what it should do: the
 * fake provider accepts everything, and any future provider that uses
 * this path must decide for itself what "no project" means rather than
 * inheriting a silent default. A UUID that matches no config row is the
 * value guaranteed to fail every real verification.
 */
private fun projectIdFromPath(): UUID =
    UUID.fromString("00000000-0000-0000-0000-000000000000")

/** Creates the Prometheus registry App.kt shares with the HTTP server and metrics. */
fun newPrometheusRegistry(): PrometheusMeterRegistry =
    PrometheusMeterRegistry(PrometheusConfig.DEFAULT)

/** Micrometer-backed [PaymentsMetrics] exposed via /metrics. */
/**
 * Register outbox depth as GAUGES fed by a supplier.
 *
 * Gauges rather than counters because the question is "how much is stuck
 * right now", and Micrometer polls the supplier at scrape time so the
 * value is never stale.
 *
 * This is the signal payments actually needs. `outbox_dispatched_total`
 * is a counter of SUCCESSES: when Kafka is unreachable it simply stops
 * increasing, and a counter that stops looks exactly like a system with
 * nothing to do. Depth goes UP when the drain is stuck, which is a
 * statement rather than an absence.
 *
 * Two series, because they answer different questions: row count says how
 * much is waiting, and the age of the oldest row distinguishes "busy"
 * from "wedged" — a large backlog that is draining has a young head.
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
    private val dispatched = registry.counter("atlas_payments_outbox_dispatched_total")
    private val depositOk = registry.counter("atlas_payments_deposits_total", "outcome", "settled")
    private val depositFail = registry.counter("atlas_payments_deposits_total", "outcome", "failed")

    override fun transactionInitiated() = initiated.increment()
    override fun transactionSettled() = settled.increment()
    override fun transactionRefunded() = refunded.increment()
    override fun reconciled(settled: Int, failed: Int, unresolved: Int) {
        if (settled > 0) reconciledSettled.increment(settled.toDouble())
        if (failed > 0) reconciledFailed.increment(failed.toDouble())
        // Always recorded, including zero, so the series exists before
        // anything goes wrong. An alert on a metric that only appears
        // during an incident cannot fire during the incident.
        reconciledUnresolved.increment(unresolved.toDouble())
    }

    override fun outboxDispatched(count: Int) = dispatched.increment(count.toDouble())
    override fun depositSettled() = depositOk.increment()
    override fun depositFailed() = depositFail.increment()
}
