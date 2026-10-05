package io.atlas.payments.stripe

import com.stripe.Stripe
import com.stripe.StripeClient
import com.stripe.exception.StripeException
import com.stripe.net.RequestOptions
import com.stripe.param.AccountCreateParams
import com.stripe.param.AccountLinkCreateParams
import io.atlas.payments.core.PaymentConfigSource
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * Connect Express onboarding: create the connected account, mint the
 * hosted onboarding link, persist the row.
 *
 * The Stripe calls happen BEFORE the database write, deliberately. If the
 * process dies after the account exists at Stripe but before the row is
 * saved, the next call creates a fresh account and the orphaned one is
 * never referenced by anything. The reverse order would leave a row
 * pointing at an account Stripe never made — which authorize would then
 * send money to.
 */
class StripeOnboarding(
    private val configs: PaymentConfigSource,
    /** Persistence for payments.stripe_accounts; null in tests that fake it. */
    private val store: ((projectId: UUID, userId: UUID, accountId: String, url: String) -> Unit)? = null,
    apiBase: String? = null,
) {
    init {
        // Same JVM-global caveat as StripePaymentProvider.
        if (apiBase != null) Stripe.overrideApiBase(apiBase)
    }

    private val clients = ConcurrentHashMap<UUID, StripeClient>()

    private fun options(projectId: UUID): RequestOptions {
        val config = configs.configFor(projectId)
            ?: throw IllegalStateException("project $projectId has no payment configuration row")
        return RequestOptions.builder().setApiKey(config.secretKey).build()
    }

    /**
     * Create (or reuse) the user's Express account and return the hosted
     * onboarding URL. `returnUrl` is where Stripe sends the user after
     * they finish (or abandon) the flow.
     */
    fun start(projectId: UUID, userId: UUID, returnUrl: String): String {
        val config = configs.configFor(projectId)
            ?: throw IllegalStateException("project $projectId has no payment configuration row")
        val client = clients.computeIfAbsent(projectId) { StripeClient(config.secretKey) }
        val opts = options(projectId)

        val accountId = try {
            // Express: Stripe hosts the identity/bank collection. Atlas
            // never touches the driver's personal data, which is the
            // compliance posture Connect exists to give us. The metadata
            // is what the account.updated webhook reads to find the row.
            client.accounts()
                .create(
                    AccountCreateParams.builder()
                        .setType(AccountCreateParams.Type.EXPRESS)
                        .putMetadata("atlas_project_id", projectId.toString())
                        .putMetadata("atlas_user_id", userId.toString())
                        .build(),
                    opts,
                ).id
        } catch (e: StripeException) {
            throw IllegalStateException(
                "Stripe refused to create the connected account: ${e.stripeError?.message ?: e.message}",
                e,
            )
        }

        val url = try {
            client.accountLinks()
                .create(
                    AccountLinkCreateParams.builder()
                        .setAccount(accountId)
                        .setRefreshUrl(returnUrl)
                        .setReturnUrl(returnUrl)
                        .setType(AccountLinkCreateParams.Type.ACCOUNT_ONBOARDING)
                        .build(),
                    opts,
                ).url
        } catch (e: StripeException) {
            throw IllegalStateException(
                "Stripe refused to mint the onboarding link: ${e.stripeError?.message ?: e.message}",
                e,
            )
        }

        store?.invoke(projectId, userId, accountId, url)
        return url
    }
}
