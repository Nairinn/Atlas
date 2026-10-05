package io.atlas.payments

import io.atlas.payments.core.ChargeRequest
import io.atlas.payments.core.PaymentProvider
import io.atlas.payments.core.ProviderResult
import io.atlas.payments.core.ProviderStatus
import io.atlas.payments.core.ProviderTimeout
import io.atlas.payments.core.RetryingPaymentProvider
import java.time.Duration
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

/**
 * Timeouts and retries around provider calls.
 *
 * The point of these is mostly the negative space: which operations are
 * deliberately NOT retried, and why that is a design decision rather than
 * an omission.
 */
class RetryingProviderTest {

    private val P1 = java.util.UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val U1 = java.util.UUID.fromString("aaaaaaaa-1111-1111-1111-111111111111")

    /** Fails its first [failures] calls, then succeeds. */
    private class FlakyProvider(
        private val failures: Int = 0,
        private val hangFor: Duration = Duration.ZERO,
    ) : PaymentProvider {
        override val name = "flaky"
        val authorizeCalls = AtomicInteger()
        val captureCalls = AtomicInteger()
        val refundCalls = AtomicInteger()
        val lookupCalls = AtomicInteger()

        private fun maybeFail(n: Int) {
            if (!hangFor.isZero) Thread.sleep(hangFor.toMillis())
            if (n <= failures) throw RuntimeException("transient provider error")
        }

        override fun authorize(request: ChargeRequest, idempotencyKey: String): ProviderResult {
            maybeFail(authorizeCalls.incrementAndGet())
            return ProviderResult(true, "ref_ok")
        }

        override fun capture(projectId: java.util.UUID, providerRef: String): ProviderResult {
            maybeFail(captureCalls.incrementAndGet())
            return ProviderResult(true, providerRef)
        }

        override fun refund(projectId: java.util.UUID, providerRef: String): ProviderResult {
            maybeFail(refundCalls.incrementAndGet())
            return ProviderResult(true, providerRef)
        }

        override fun lookup(projectId: java.util.UUID, providerRef: String): ProviderStatus {
            maybeFail(lookupCalls.incrementAndGet())
            return ProviderStatus.CAPTURED
        }

        override fun verifyWebhook(projectId: java.util.UUID, payload: String, signature: String?) = true
    }

    private fun wrap(
        p: PaymentProvider,
        attempts: Int = 3,
        timeout: Duration = Duration.ofSeconds(5),
    ) = RetryingPaymentProvider(
        delegate = p,
        maxAttempts = attempts,
        timeout = timeout,
        backoff = Duration.ofMillis(1),
        // Tests must not actually sleep through the backoff.
        sleeper = {},
    )

    // --- what is retried --------------------------------------------------

    /**
     * authorize carries the caller's idempotency key, which is precisely
     * what makes a replay safe: the provider returns the original result
     * rather than charging again.
     */
    @Test
    fun `authorize is retried and eventually succeeds`() {
        val flaky = FlakyProvider(failures = 2)
        val result = wrap(flaky).authorize(ChargeRequest(P1, U1, 1_000), "key-1")

        assertTrue(result.success)
        assertEquals(3, flaky.authorizeCalls.get(), "two failures, then success")
    }

    /** lookup asks a question and changes nothing, so it is always safe. */
    @Test
    fun `lookup is retried`() {
        val flaky = FlakyProvider(failures = 1)
        assertEquals(ProviderStatus.CAPTURED, wrap(flaky).lookup(P1, "ref"))
        assertEquals(2, flaky.lookupCalls.get())
    }

    @Test
    fun `retries are bounded and the last error surfaces`() {
        val flaky = FlakyProvider(failures = 99)
        assertFailsWith<RuntimeException> { wrap(flaky, attempts = 3).authorize(ChargeRequest(P1, U1, 1_000), "k") }
        assertEquals(3, flaky.authorizeCalls.get(), "no more than maxAttempts")
    }

    // --- what is deliberately NOT retried ---------------------------------

    /**
     * capture and refund identify a charge but carry no idempotency key of
     * their own. Some providers treat a repeat as a no-op; others do not,
     * and "usually idempotent" is not something to bet customer money on.
     * A double refund is a real loss and a hard one to notice.
     *
     * The recovery path for an ambiguous capture is the reconciliation
     * sweep, which asks the provider what happened instead of repeating
     * the write and hoping.
     */
    @Test
    fun `capture is attempted exactly once`() {
        val flaky = FlakyProvider(failures = 99)
        assertFailsWith<RuntimeException> { wrap(flaky).capture(P1, "ref") }
        assertEquals(1, flaky.captureCalls.get(), "a retried capture could charge twice")
    }

    @Test
    fun `refund is attempted exactly once`() {
        val flaky = FlakyProvider(failures = 99)
        assertFailsWith<RuntimeException> { wrap(flaky).refund(P1, "ref") }
        assertEquals(1, flaky.refundCalls.get(), "a retried refund could pay out twice")
    }

    // --- timeouts ---------------------------------------------------------

    /**
     * The failure mode of an unbounded provider call is worse than an
     * error: the request thread parks, the connection pool drains behind
     * it, and a slow processor becomes an outage in a healthy service.
     */
    @Test
    fun `a hanging provider call times out rather than parking forever`() {
        val slow = FlakyProvider(hangFor = Duration.ofSeconds(30))
        val started = System.currentTimeMillis()

        assertFailsWith<ProviderTimeout> {
            wrap(slow, attempts = 1, timeout = Duration.ofMillis(200)).capture(P1, "ref")
        }

        val elapsed = System.currentTimeMillis() - started
        assertTrue(elapsed < 5_000, "should have given up quickly, took ${elapsed}ms")
    }

    @Test
    fun `a timeout on a retryable call is retried, then gives up`() {
        val slow = FlakyProvider(hangFor = Duration.ofSeconds(30))
        assertFailsWith<ProviderTimeout> {
            wrap(slow, attempts = 2, timeout = Duration.ofMillis(150)).authorize(ChargeRequest(P1, U1, 100), "k")
        }
    }

    @Test
    fun `webhook verification is not wrapped in a network timeout`() {
        // It is local and cheap; routing it through the executor would add
        // a thread hop to every webhook for nothing.
        assertTrue(wrap(FlakyProvider()).verifyWebhook(P1, "{}", "sig"))
    }
}
