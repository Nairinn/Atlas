package io.atlas.payments.stripe

import com.stripe.Stripe
import com.stripe.StripeClient
import com.stripe.exception.StripeException
import com.stripe.net.RequestOptions
import com.stripe.param.PaymentIntentCaptureParams
import com.stripe.param.PaymentIntentCreateParams
import com.stripe.param.RefundCreateParams
import io.atlas.payments.core.ChargeRequest
import io.atlas.payments.core.HmacWebhookVerifier
import io.atlas.payments.core.PaymentConfigSource
import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.PaymentProvider
import io.atlas.payments.core.ProviderResult
import io.atlas.payments.core.ProviderStatus
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * Stripe behind [PaymentProvider], one tenant's credentials at a time.
 *
 * # Connect, in this adapter's terms
 *
 * Atlas never holds funds. Each project connects its own Stripe account
 * (the `control.project_payment_config` row), and each payee — a ride's
 * driver — gets a Stripe Express account under it. A fare is a
 * destination charge:
 *
 *   - [authorize] creates a PaymentIntent on the TENANT's account with
 *     `capture_method=manual`, `transfer_data.destination` = the driver's
 *     connected account, `application_fee_amount` = the tenant's cut.
 *   - [capture] takes the rider's money and moves the driver's share to
 *     their connected account in the same operation; the fee stays with
 *     the tenant. Payouts from there are Stripe's business, which is why
 *     WITHDRAWAL does not exist in Atlas.
 *
 * # Per-project clients
 *
 * `StripeClient` is constructed per credential rather than set globally
 * (the pre-v26 static `Stripe.apiKey`): one deployment serves many
 * tenants, and a global would be the last tenant's key charging every
 * tenant's customers. Clients are cached per project — construction is
 * cheap but not free, and the authorize path touches one on every charge.
 * The API key is also passed per request via [RequestOptions], because
 * the client cache is only a convenience; the request options are what
 * actually guarantee the right credential is used.
 *
 * # Testing
 *
 * `STRIPE_API_BASE` points this adapter at stripe-mock or a test double
 * via `Stripe.overrideApiBase`, applied once in the constructor — it is a
 * JVM-global static in stripe-java, so it must not be toggled per call.
 *
 * # What is deliberately NOT retried here
 *
 * Nothing: [io.atlas.payments.core.RetryingPaymentProvider] owns retry
 * policy, and Stripe's own `maxNetworkRetries` is left at 0 so the two
 * layers cannot double-retry (two layers each retrying three times is
 * nine attempts, and the backoff of one layer multiplies the other's).
 */
class StripePaymentProvider(
    private val configs: PaymentConfigSource,
    /** Overrides where API calls go; null means api.stripe.com. */
    apiBase: String? = null,
) : PaymentProvider {

    override val name: String = "stripe"

    /** The override captured at construction; see the init note. */
    private var stripeApiBase: String? = null

    init {
        // stripe-java 34's StripeClient builder HARDCODES the api base; the
        // Stripe.overrideApiBase static only affects the legacy global API.
        // Both are set so any entry path hits the override (stripe-mock).
        if (apiBase != null) {
            Stripe.overrideApiBase(apiBase)
            stripeApiBase = apiBase
        } else {
            stripeApiBase = null
        }
    }

    private val clients = ConcurrentHashMap<UUID, StripeClient>()

    private fun configFor(projectId: UUID) =
        configs.configFor(projectId)
            ?: throw IllegalStateException(
                "project $projectId has no payment configuration row; " +
                    "a charge cannot be placed on an account that was never connected",
            )

    private fun clientFor(projectId: UUID): StripeClient =
        clients.computeIfAbsent(projectId) { key ->
            val config = configFor(key)
            val builder = StripeClient.builder().setApiKey(config.secretKey)
            stripeApiBase?.let { builder.setApiBase(it) }
            builder.build()
        }

    private fun options(projectId: UUID, idempotencyKey: String? = null): RequestOptions {
        val builder = RequestOptions.builder().setApiKey(configFor(projectId).secretKey)
        // The caller's key becomes Stripe's Idempotency-Key header, which
        // is what makes a retried authorize return the original
        // PaymentIntent instead of creating a second charge.
        idempotencyKey?.let { builder.setIdempotencyKey(it) }
        return builder.build()
    }

    override fun authorize(request: ChargeRequest, idempotencyKey: String): ProviderResult {
        val config = configFor(request.projectId)

        val params = PaymentIntentCreateParams.builder()
            .setAmount(request.amountCents)
            .setCurrency(config.currency)
            // Manual capture: capture happens at settlement, when the ride
            // completes. The client confirms the payment with Stripe's
            // client SDK using the returned client_secret (D1); the server
            // only captures.
            .setCaptureMethod(PaymentIntentCreateParams.CaptureMethod.MANUAL)
            .putMetadata("atlas_project_id", request.projectId.toString())
            .putMetadata("atlas_user_id", request.userId.toString())

        // A fare pays the driver's connected account; a deposit pays
        // nobody. The service already resolved (and checked) the payee's
        // destination; there is nothing to re-verify here.
        val destination = request.destinationAccountId
        if (destination != null) {
            params.setTransferData(
                PaymentIntentCreateParams.TransferData.builder()
                    .setDestination(destination)
                    .build(),
            )
            request.applicationFeeCents?.let { params.setApplicationFeeAmount(it) }
        }

        return try {
            val intent = clientFor(request.projectId)
                .paymentIntents().create(params.build(), options(request.projectId, idempotencyKey))
            ProviderResult(success = true, providerRef = intent.id, clientSecret = intent.clientSecret)
        } catch (e: StripeException) {
            ProviderResult(success = false, providerRef = "", message = e.stripeError?.message ?: e.message)
        }
    }

    override fun capture(projectId: UUID, providerRef: String): ProviderResult = try {
        val intent = clientFor(projectId)
            .paymentIntents()
            .capture(providerRef, PaymentIntentCaptureParams.builder().build(), options(projectId))
        // PaymentIntent.status is a bare String in this SDK version;
        // the literals are Stripe's API values, stable across versions.
        ProviderResult(success = intent.status == "succeeded", providerRef = intent.id)
    } catch (e: StripeException) {
        ProviderResult(success = false, providerRef = providerRef, message = e.stripeError?.message ?: e.message)
    }

    override fun cancel(projectId: UUID, providerRef: String): ProviderResult = try {
        val intent = clientFor(projectId)
            .paymentIntents()
            .cancel(providerRef, options(projectId))
        ProviderResult(success = intent.status == "canceled", providerRef = intent.id)
    } catch (e: StripeException) {
        ProviderResult(success = false, providerRef = providerRef, message = e.stripeError?.message ?: e.message)
    }

    override fun refund(projectId: UUID, providerRef: String): ProviderResult = try {
        val refund = clientFor(projectId)
            .refunds()
            .create(
                RefundCreateParams.builder().setPaymentIntent(providerRef).build(),
                options(projectId),
            )
        ProviderResult(success = refund.status != "failed", providerRef = providerRef)
    } catch (e: StripeException) {
        ProviderResult(success = false, providerRef = providerRef, message = e.stripeError?.message ?: e.message)
    }

    override fun clientSecret(projectId: UUID, providerRef: String): String? = try {
        clientFor(projectId).paymentIntents().retrieve(providerRef, options(projectId)).clientSecret
    } catch (e: StripeException) {
        null
    }

    override fun lookup(projectId: UUID, providerRef: String): ProviderStatus = try {
        when (clientFor(projectId).paymentIntents().retrieve(providerRef, options(projectId)).status) {
            "succeeded" -> ProviderStatus.CAPTURED
            "requires_capture" -> ProviderStatus.AUTHORIZED
            "requires_confirmation", "requires_action" -> ProviderStatus.AWAITING_CUSTOMER
            "requires_payment_method" -> ProviderStatus.FAILED
            "canceled" -> ProviderStatus.FAILED
            else -> ProviderStatus.UNKNOWN
        }
    } catch (e: StripeException) {
        // A 404 from Stripe is NOT_FOUND; everything else (network,
        // auth, rate limit) is genuinely unknown and must leave the
        // transaction alone — the sweep's contract.
        if (e.stripeError?.code == "resource_missing") ProviderStatus.NOT_FOUND
        else ProviderStatus.UNKNOWN
    }

    override fun verifyWebhook(projectId: UUID, payload: String, signature: String?): Boolean {
        val config = configs.configFor(projectId) ?: return false
        // HmacWebhookVerifier implements Stripe's v1 scheme exactly
        // (t=…,v1=…, HMAC-SHA256 over "t.payload", constant-time compare,
        // replay window). Kept rather than the SDK's Webhook.constructEvent
        // because it is already tested and because it verifies without
        // parsing — a signature must be checked against the RAW body.
        return HmacWebhookVerifier(config.webhookSecret).verify(payload, signature)
    }
}
