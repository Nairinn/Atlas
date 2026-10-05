package io.atlas.payments.core

import java.util.UUID

/**
 * Everything a provider needs to place one charge.
 *
 * The old interface took bare (amount, key): enough for a provider that
 * approves everything, not enough for a real one. A Stripe charge needs to
 * know whose Stripe account it is created on (the project's — Connect
 * means the tenant is the merchant of record), who pays, who gets paid,
 * and what the tenant's cut is. Passing a single object rather than
 * growing the parameter list keeps every future field an additive change
 * at the call sites that opt in, and a compile error nowhere.
 */
data class ChargeRequest(
    /** The tenant. Resolves the processor credentials to use. */
    val projectId: UUID,
    /** The payer, for metadata and customer linkage. */
    val userId: UUID,
    val amountCents: Long,
    /**
     * The payee's processor account (a Stripe connected account id,
     * `acct_…`) when this charge pays somebody — a ride fare's driver.
     *
     * Null for deposits: money arriving from outside has no internal
     * destination beyond the payer's own wallet.
     */
    val destinationAccountId: String? = null,
    /**
     * The tenant's cut, in cents, taken OUT of [amountCents] rather than
     * added on top. Null means no fee. Must be < [amountCents]; the
     * service validates that before a provider ever sees it.
     */
    val applicationFeeCents: Long? = null,
)

/**
 * Abstraction over an external payment processor (Stripe, Adyen, ...).
 *
 * # Everything real except the network call
 *
 * The platform deliberately runs against [FakePaymentProvider] by default.
 * That is a placeholder for the *provider call only* — not for the
 * machinery around it. Idempotency, the pending-then-capture recovery
 * window, the transactional outbox, the ledger updates and the webhook
 * path are all real and exercised, because those are the parts that are
 * expensive to get wrong and expensive to retrofit.
 *
 * Swapping in a real processor means implementing this interface. Nothing
 * above it moves. Stripe's test mode is free, so even that work costs
 * nothing until live keys are used.
 *
 * # Contract
 *
 * Provider calls are made OUTSIDE the database transaction: network I/O
 * must never hold a Postgres row lock. Implementations must be safe to
 * retry with the same idempotency key — [authorize] is called with the
 * caller's key precisely so a retry does not double-charge.
 *
 * # Tenancy
 *
 * The money methods take the project FIRST. Each tenant connects its OWN
 * processor account (Stripe Connect: the tenant is the merchant of record
 * and Atlas never holds funds), so credentials, currency, and payout
 * destinations are per project. A provider that resolved nothing per
 * project would charge every tenant's customers against one account —
 * the payments equivalent of the cross-tenant hole migration 0080 closed
 * for wallets.
 */
interface PaymentProvider {
    /** Human-readable name, surfaced in logs and on the health endpoint. */
    val name: String

    /** Reserve [ChargeRequest.amountCents] against the payer. Returns the provider reference. */
    fun authorize(request: ChargeRequest, idempotencyKey: String): ProviderResult

    /** Capture a previously authorized charge. */
    fun capture(projectId: UUID, providerRef: String): ProviderResult

    /** Reverse a captured charge. */
    fun refund(projectId: UUID, providerRef: String): ProviderResult

    /**
     * Ask the provider what actually happened to a charge.
     *
     * Needed for reconciliation, and it is the only honest way to resolve
     * a transaction stuck in PENDING. A pending row means Atlas started a
     * charge and never recorded the outcome — the process died between
     * authorize and capture, or the provider timed out after doing the
     * work. Guessing either way is wrong: assume success and you credit a
     * wallet against money that was never collected, assume failure and
     * you refuse a payment the customer has already made.
     *
     * Implementations must not have side effects. This asks a question.
     */
    fun lookup(projectId: UUID, providerRef: String): ProviderStatus

    /**
     * Verify that a webhook body genuinely came from the provider, for the
     * project whose endpoint received it.
     *
     * This exists on the interface rather than in the HTTP layer because
     * the verification scheme is provider-specific — Stripe signs with an
     * HMAC over a timestamped payload, others use mTLS or a shared token.
     * Putting it here means the endpoint cannot accidentally ship without
     * a check: it has something to call from day one, and the real
     * implementation slots in behind it.
     *
     * An unverified webhook is an unauthenticated write from the public
     * internet into a payments system, so the endpoint MUST reject when
     * this returns false.
     */
    fun verifyWebhook(projectId: UUID, payload: String, signature: String?): Boolean
}

/**
 * What the provider says a charge's real state is.
 *
 * `UNKNOWN` is deliberately not folded into `FAILED`. A provider that
 * cannot be reached, or that returns something this adapter does not
 * recognise, has told us nothing — and "nothing" must leave the
 * transaction alone for a human to look at rather than resolving it in
 * whichever direction happens to be convenient.
 */
enum class ProviderStatus {
    /** Authorized but not captured. Still in flight. */
    AUTHORIZED,

    /** Money was taken. */
    CAPTURED,

    /** The provider declined, or the charge was cancelled. */
    FAILED,

    /** The provider has no record of this reference at all. */
    NOT_FOUND,

    /** Could not be determined. Resolve nothing; escalate. */
    UNKNOWN,
}

data class ProviderResult(
    val success: Boolean,
    val providerRef: String,
    val message: String? = null,
)

/**
 * Always-approve provider for local development and tests.
 *
 * Generates a unique `fake_*` reference on authorize and echoes it back on
 * capture and refund, so the full authorize -> capture -> refund flow is
 * exercised end to end without a network call or a cent of real money.
 *
 * The project parameter is accepted and ignored: the fake has one
 * behaviour for every tenant, which is precisely the property that makes
 * it unsafe anywhere but local dev.
 */
class FakePaymentProvider : PaymentProvider {
    override val name: String = "fake"

    override fun authorize(request: ChargeRequest, idempotencyKey: String): ProviderResult =
        ProviderResult(success = true, providerRef = "fake_${UUID.randomUUID()}")

    override fun capture(projectId: UUID, providerRef: String): ProviderResult =
        ProviderResult(success = true, providerRef = providerRef)

    override fun refund(projectId: UUID, providerRef: String): ProviderResult =
        ProviderResult(success = true, providerRef = providerRef)

    /**
     * Reports CAPTURED for anything it minted.
     *
     * The fake never loses a charge, so reconciliation against it always
     * resolves cleanly. That is fine for exercising the sweep's mechanics
     * and useless for exercising its judgement — which is why the sweep's
     * tests drive a provider they control rather than this one.
     */
    override fun lookup(projectId: UUID, providerRef: String): ProviderStatus =
        if (providerRef.startsWith("fake_")) ProviderStatus.CAPTURED
        else ProviderStatus.NOT_FOUND

    /**
     * Accepts anything.
     *
     * Safe only because this provider never sends webhooks, so nothing
     * legitimate calls the endpoint at all. It is NOT a stand-in for real
     * verification: shipping this against a live processor would leave an
     * unauthenticated write path into payments.
     */
    override fun verifyWebhook(projectId: UUID, payload: String, signature: String?): Boolean = true
}

/**
 * Chooses the provider from configuration.
 *
 * Unknown values fail loudly at startup rather than silently falling back
 * to the fake. A service that quietly runs on a stub provider in
 * production because of a typo in an environment variable would approve
 * every charge and move real balances against money that was never
 * collected.
 */
object PaymentProviders {
    fun fromName(name: String): PaymentProvider = when (name.trim().lowercase()) {
        "", "fake" -> FakePaymentProvider()
        else -> throw IllegalArgumentException(
            "unknown PAYMENT_PROVIDER '$name'. Known: fake. " +
                "Stripe is wired per project via control.project_payment_config, " +
                "not by name — see StripePaymentProvider.",
        )
    }
}
