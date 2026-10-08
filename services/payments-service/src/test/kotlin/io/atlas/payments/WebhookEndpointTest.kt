package io.atlas.payments

import io.atlas.payments.core.ChargeRequest
import io.atlas.payments.core.PaymentProvider
import io.atlas.payments.core.PaymentsMetrics
import io.atlas.payments.core.ProviderResult
import io.atlas.payments.core.ProviderStatus
import io.atlas.payments.http.newPrometheusRegistry
import io.atlas.payments.http.startHttpServer
import java.net.HttpURLConnection
import java.net.ServerSocket
import java.net.URI
import java.util.UUID
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * The webhook endpoint, against the real Netty server.
 *
 * Not a harness: `startHttpServer` is the same function App.kt calls, on a
 * real port, over real HTTP. The thing being tested is the WIRING — that
 * a `false` from the verifier actually stops the request — and a harness
 * that stubbed out the server could not tell me that.
 *
 * This gap was worth closing because `FakePaymentProvider.verifyWebhook`
 * returns true unconditionally, so every existing test exercised only the
 * accepting path. The rejecting path is the one carrying the security
 * property.
 */
class WebhookEndpointTest {

    /** Any UUID parses; the fake providers ignore which project it names. */
    private val PROJECT = UUID.randomUUID()

    /** Says no to everything, and records that it was asked. */
    private class RejectingProvider : PaymentProvider {
        override val name = "rejecting"
        var asked = 0
            private set

        override fun authorize(request: ChargeRequest, idempotencyKey: String) =
            ProviderResult(true, "ref")

        override fun capture(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun refund(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun cancel(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun clientSecret(projectId: UUID, providerRef: String): String? = null
        override fun lookup(projectId: UUID, providerRef: String) = ProviderStatus.UNKNOWN

        override fun verifyWebhook(projectId: UUID, payload: String, signature: String?): Boolean {
            asked++
            return false
        }
    }

    private class AcceptingProvider : PaymentProvider {
        override val name = "accepting"
        var lastPayload: String? = null
            private set

        override fun authorize(request: ChargeRequest, idempotencyKey: String) =
            ProviderResult(true, "ref")

        override fun capture(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun refund(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun cancel(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun clientSecret(projectId: UUID, providerRef: String): String? = null
        override fun lookup(projectId: UUID, providerRef: String) = ProviderStatus.UNKNOWN

        override fun verifyWebhook(projectId: UUID, payload: String, signature: String?): Boolean {
            lastPayload = payload
            return true
        }
    }

    private var engine: io.ktor.server.netty.NettyApplicationEngine? = null

    @AfterTest
    fun stop() {
        engine?.stop(0, 0)
    }

    private fun serve(provider: PaymentProvider): Int {
        // A port the OS just told us was free. Racy in principle, fine in
        // practice, and far simpler than plumbing Ktor's resolved port out.
        val port = ServerSocket(0).use { it.localPort }
        engine = startHttpServer(port, newPrometheusRegistry(), provider, PaymentsMetrics.NOOP)
        // Wait for the listener rather than sleeping a fixed amount.
        val deadline = System.currentTimeMillis() + 10_000
        while (System.currentTimeMillis() < deadline) {
            try {
                java.net.Socket("127.0.0.1", port).close()
                return port
            } catch (e: Exception) {
                Thread.sleep(25)
            }
        }
        error("server did not start on :$port")
    }

    private fun post(port: Int, body: String, signature: String? = null): Int {
        val conn = URI("http://127.0.0.1:$port/webhooks/stripe/$PROJECT").toURL()
            .openConnection() as HttpURLConnection
        conn.requestMethod = "POST"
        conn.doOutput = true
        conn.setRequestProperty("Content-Type", "application/json")
        signature?.let { conn.setRequestProperty("Stripe-Signature", it) }
        conn.outputStream.use { it.write(body.toByteArray()) }
        return conn.responseCode
    }

    /**
     * The security property. An unverified webhook is an unauthenticated
     * write from the public internet into a payments system, so a false
     * from the verifier must stop the request — not be logged and ignored.
     */
    @Test
    fun `a webhook the provider will not verify is rejected`() {
        val provider = RejectingProvider()
        val port = serve(provider)

        assertEquals(401, post(port, """{"id":"evt_1"}""", "t=1,v1=deadbeef"))
        assertTrue(provider.asked > 0, "the endpoint must actually consult the verifier")
    }

    /** And a signature-less request is refused the same way. */
    @Test
    fun `a webhook with no signature header is rejected`() {
        val port = serve(RejectingProvider())
        assertEquals(401, post(port, """{"id":"evt_1"}"""))
    }

    @Test
    fun `a verified webhook with no handler is acknowledged as 202`() {
        val provider = AcceptingProvider()
        val port = serve(provider)

        // Verified, but no handler wired in this harness: an honest 202
        // rather than a 200 that pretends the event was acted on.
        assertEquals(202, post(port, """{"id":"evt_1"}""", "t=1,v1=whatever"))
        assertEquals(
            """{"id":"evt_1"}""",
            provider.lastPayload,
            "the verifier must see the RAW body: a signature covers exact bytes",
        )
    }

    /**
     * The body is read before it can be verified, because the signature
     * covers the body. That makes an unbounded read a way for anyone on
     * the internet to make the service allocate as much as they like.
     */
    @Test
    fun `an oversized webhook is refused before verification`() {
        val provider = RejectingProvider()
        val port = serve(provider)
        val huge = "x".repeat(2_000_000)

        assertEquals(413, post(port, huge, "t=1,v1=abc"))
        assertEquals(0, provider.asked, "there is no point verifying what we refuse to hold")
    }

    @Test
    fun `a malformed project id is a 400`() {
        val port = serve(RejectingProvider())
        val conn = URI("http://127.0.0.1:$port/webhooks/stripe/not-a-uuid").toURL()
            .openConnection() as HttpURLConnection
        conn.requestMethod = "POST"
        conn.doOutput = true
        conn.outputStream.use { it.write("{}".toByteArray()) }
        assertEquals(400, conn.responseCode)
    }
}
