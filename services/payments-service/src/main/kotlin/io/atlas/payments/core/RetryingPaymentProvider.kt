package io.atlas.payments.core

import org.slf4j.LoggerFactory
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import java.time.Duration

/**
 * Adds bounded timeouts and retries around any [PaymentProvider].
 *
 * # Not everything here is retried, on purpose
 *
 * Retrying a payment operation is only safe when the provider can tell
 * that the second call is the same call. That is true of two of the four:
 *
 *  - [authorize] carries the caller's idempotency key, which is exactly
 *    what makes a replay safe — the provider returns the original result
 *    instead of charging again.
 *  - [lookup] asks a question and changes nothing.
 *
 * The other two are NOT retried:
 *
 *  - [capture] and [refund] identify the charge but carry no idempotency
 *    key of their own. Some providers treat a repeat as a no-op; others
 *    do not, and "usually idempotent" is not a property to bet customer
 *    money on. A double refund is a real loss and a hard one to notice.
 *
 * That is not a gap. A capture whose response was lost leaves the
 * transaction PENDING, and [ReconciliationSweep] resolves it by asking
 * the provider what actually happened — which is the correct way to
 * recover from an ambiguous write, rather than repeating it and hoping.
 *
 * # Timeouts
 *
 * Every call is bounded, because the failure mode of an unbounded
 * provider call is worse than an error: the request thread parks, the
 * connection pool drains behind it, and a slow processor becomes an
 * outage in a service that is otherwise healthy.
 */
class RetryingPaymentProvider(
    private val delegate: PaymentProvider,
    private val maxAttempts: Int = 3,
    private val timeout: Duration = Duration.ofSeconds(10),
    private val backoff: Duration = Duration.ofMillis(200),
    private val sleeper: (Duration) -> Unit = { Thread.sleep(it.toMillis()) },
) : PaymentProvider {

    init {
        require(maxAttempts >= 1) { "maxAttempts must be at least 1" }
    }

    override val name: String = "${delegate.name} (retrying)"

    override fun authorize(request: ChargeRequest, idempotencyKey: String): ProviderResult =
        withRetries("authorize") { delegate.authorize(request, idempotencyKey) }

    override fun lookup(projectId: java.util.UUID, providerRef: String): ProviderStatus =
        withRetries("lookup") { delegate.lookup(projectId, providerRef) }

    override fun clientSecret(projectId: java.util.UUID, providerRef: String): String? =
        withRetries("clientSecret") { delegate.clientSecret(projectId, providerRef) }

    /** Bounded by a timeout, but attempted once. See the class note. */
    override fun capture(projectId: java.util.UUID, providerRef: String): ProviderResult =
        withTimeout("capture") { delegate.capture(projectId, providerRef) }

    /** Bounded by a timeout, but attempted once. See the class note. */
    override fun cancel(projectId: java.util.UUID, providerRef: String): ProviderResult =
        withTimeout("cancel") { delegate.cancel(projectId, providerRef) }

    /** Bounded by a timeout, but attempted once. See the class note. */
    override fun refund(projectId: java.util.UUID, providerRef: String): ProviderResult =
        withTimeout("refund") { delegate.refund(projectId, providerRef) }

    /** Local, cheap, and not a network call. */
    override fun verifyWebhook(projectId: java.util.UUID, payload: String, signature: String?): Boolean =
        delegate.verifyWebhook(projectId, payload, signature)

    private fun <T> withRetries(op: String, call: () -> T): T {
        var last: Exception? = null
        for (attempt in 1..maxAttempts) {
            try {
                return withTimeout(op, call)
            } catch (e: Exception) {
                last = e
                if (attempt == maxAttempts) break
                // Exponential, so a provider that is briefly overloaded is
                // not hammered at a fixed interval by every instance at
                // once.
                val wait = backoff.multipliedBy(1L shl (attempt - 1))
                LOG.warn(
                    "provider {} attempt {}/{} failed ({}); retrying in {}ms",
                    op, attempt, maxAttempts, e.message, wait.toMillis(),
                )
                sleeper(wait)
            }
        }
        throw last ?: IllegalStateException("no attempt was made")
    }

    private fun <T> withTimeout(op: String, call: () -> T): T {
        val future = EXECUTOR.submit<T> { call() }
        return try {
            future.get(timeout.toMillis(), TimeUnit.MILLISECONDS)
        } catch (e: TimeoutException) {
            // Cancel so the worker is not left running against a provider
            // nobody is waiting for any more.
            future.cancel(true)
            throw ProviderTimeout("provider $op timed out after $timeout")
        } catch (e: java.util.concurrent.ExecutionException) {
            // Unwrap so callers see the provider's own error rather than
            // the executor's wrapper.
            throw (e.cause as? Exception) ?: e
        }
    }

    private companion object {
        private val LOG = LoggerFactory.getLogger(RetryingPaymentProvider::class.java)

        /**
         * Daemon threads: a stuck provider call must never be the reason
         * the JVM refuses to shut down during a deploy.
         */
        private val EXECUTOR = Executors.newCachedThreadPool { r ->
            Thread(r, "payment-provider").apply { isDaemon = true }
        }
    }
}

/** A provider call that exceeded its deadline. */
class ProviderTimeout(message: String) : RuntimeException(message)
