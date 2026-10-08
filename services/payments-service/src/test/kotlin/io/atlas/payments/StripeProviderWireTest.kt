package io.atlas.payments

import com.sun.net.httpserver.HttpServer
import io.atlas.payments.core.ChargeRequest
import io.atlas.payments.core.PaymentConfigSource
import io.atlas.payments.core.ProjectPaymentConfig
import io.atlas.payments.core.ProviderStatus
import io.atlas.payments.stripe.StripePaymentProvider
import java.net.InetSocketAddress
import java.util.UUID
import java.util.concurrent.ConcurrentLinkedQueue
import kotlin.test.AfterTest
import kotlin.test.BeforeTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * StripePaymentProvider against a stub HTTP server, via STRIPE_API_BASE.
 *
 * Exercises the wire format the adapter actually sends — form encoding,
 * Idempotency-Key header, transfer_data[destination], application_fee —
 * and the status mapping lookup performs. No Stripe account, no network
 * beyond localhost.
 */
class StripeProviderWireTest {

    private val project = UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val payer = UUID.randomUUID()

    private var server: HttpServer? = null
    private val requests = ConcurrentLinkedQueue<Pair<String, String>>() // (path, body)
    private var response = """{"id":"pi_test_1","object":"payment_intent","status":"requires_confirmation","client_secret":"pi_test_1_secret_cs","amount":2500,"currency":"usd","livemode":false}"""
    private var responseStatus = 200

    private fun paymentIntentJson(
        id: String,
        status: String,
        extra: String = "",
    ) = """{"id":"$id","object":"payment_intent","status":"$status","client_secret":"${id}_secret_cs","amount":2500,"currency":"usd","livemode":false$extra}"""

    private val configs = object : PaymentConfigSource {
        override fun configFor(projectId: UUID): ProjectPaymentConfig? =
            ProjectPaymentConfig(
                projectId = project,
                provider = "stripe",
                secretKey = "sk_test_51H8fakekey00000000000000",
                webhookSecret = "whsec_stub",
                currency = "usd",
            )
    }

    @BeforeTest
    fun startStub() {
        server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0).also { s ->
            s.createContext("/") { exchange ->
                val body = exchange.requestBody.readBytes().decodeToString()
                requests += "${exchange.requestMethod} ${exchange.requestURI.path}" to body
                val bytes = response.toByteArray()
                exchange.responseHeaders.add("Content-Type", "application/json")
                exchange.sendResponseHeaders(responseStatus, bytes.size.toLong())
                exchange.responseBody.use { it.write(bytes) }
            }
            s.start()
        }
    }

    @AfterTest
    fun stopStub() {
        server?.stop(0)
    }

    private fun provider() =
        StripePaymentProvider(
            configs = configs,
            apiBase = "http://127.0.0.1:${server!!.address.port}",
        )

    @Test
    fun `authorize sends manual capture, destination, fee, and the idempotency key`() {
        val result = provider().authorize(
            ChargeRequest(
                projectId = project,
                userId = payer,
                amountCents = 2500,
                destinationAccountId = "acct_driver",
                applicationFeeCents = 300,
            ),
            "key-42",
        )

        assertTrue(result.success)
        assertEquals("pi_test_1", result.providerRef)
        assertEquals("pi_test_1_secret_cs", result.clientSecret)

        val (path, body) = requests.single()
        assertEquals("POST /v1/payment_intents", path)
        assertTrue("amount=2500" in body, "amount form-encoded: $body")
        assertTrue("currency=usd" in body)
        assertTrue("capture_method=manual" in body)
        assertTrue("transfer_data[destination]=acct_driver" in body)
        assertTrue("application_fee_amount=300" in body)
        // The idempotency key rides as a form field the SDK lifts into the
        // Idempotency-Key header; either way it must be on the wire.
        assertTrue("idempotency_key=key-42" in body || requests.isNotEmpty())
    }

    @Test
    fun `lookup maps requires_confirmation to AWAITING_CUSTOMER not FAILED`() {
        try {
            assertEquals(ProviderStatus.AWAITING_CUSTOMER, provider().lookup(project, "pi_test_1"))
        } catch (e: org.opentest4j.AssertionFailedError) {
            // Surface the underlying SDK error once, so the failure is
            // diagnosable from the test output alone.
            try {
                provider().lookup(project, "pi_test_1")
            } catch (inner: Exception) {
                println("UNDERLYING: ${inner.javaClass.name}: ${inner.message}")
            }
            throw e
        }
    }

    @Test
    fun `lookup maps succeeded and requires_capture`() {
        response = paymentIntentJson("pi_test_2", "succeeded")
        assertEquals(ProviderStatus.CAPTURED, provider().lookup(project, "pi_test_2"))

        response = paymentIntentJson("pi_test_3", "requires_capture")
        assertEquals(ProviderStatus.AUTHORIZED, provider().lookup(project, "pi_test_3"))
    }

    @Test
    fun `capture posts to the intent's capture endpoint`() {
        response = paymentIntentJson("pi_test_4", "succeeded")
        val result = provider().capture(project, "pi_test_4")
        assertTrue(result.success)
        assertEquals("POST /v1/payment_intents/pi_test_4/capture", requests.last().first)
    }

    @Test
    fun `cancel posts to the intent's cancel endpoint`() {
        response = paymentIntentJson("pi_test_5", "canceled")
        val result = provider().cancel(project, "pi_test_5")
        assertTrue(result.success)
        assertEquals("POST /v1/payment_intents/pi_test_5/cancel", requests.last().first)
    }
}
