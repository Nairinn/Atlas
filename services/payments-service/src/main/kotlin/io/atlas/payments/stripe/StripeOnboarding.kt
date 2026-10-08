package io.atlas.payments.stripe

import com.stripe.Stripe
import com.stripe.StripeClient
import com.stripe.exception.StripeException
import com.stripe.net.RequestOptions
import com.stripe.param.AccountCreateParams
import com.stripe.param.AccountLinkCreateParams
import io.atlas.payments.core.PayoutAccount
import io.atlas.payments.core.PaymentConfigSource
import io.atlas.payments.core.PaymentError
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * Connect Express onboarding: create the connected account (once), mint
 * the hosted onboarding link, persist the row.
 *
 * An existing account is REUSED: a second call mints a fresh AccountLink
 * for the stored account id rather than creating another Stripe account,
 * because every create leaves an orphan at Stripe that no Atlas row ever
 * references again.
 */
class StripeOnboarding(
    private val configs: PaymentConfigSource,
    /** Reads and persists payments.stripe_accounts; null in tests that fake it. */
    private val accounts: AccountStore? = null,
    apiBase: String? = null,
) {
    interface AccountStore {
        fun find(projectId: UUID, userId: UUID): PayoutAccount?

        fun save(projectId: UUID, userId: UUID, stripeAccountId: String, onboardingUrl: String)
    }

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

    private fun clientAndOptions(projectId: UUID): Pair<StripeClient, RequestOptions> {
        val config = configs.configFor(projectId)
            ?: throw PaymentError.InvalidState("project $projectId has no payment configuration row")
        val client = clients.computeIfAbsent(projectId) {
            val builder = StripeClient.builder().setApiKey(config.secretKey)
            stripeApiBase?.let { b -> builder.setApiBase(b) }
            builder.build()
        }
        return client to RequestOptions.builder().setApiKey(config.secretKey).build()
    }

    /**
     * Create (or reuse) the user's Express account and return the hosted
     * onboarding URL. `returnUrl` is where Stripe sends the user after
     * they finish (or abandon) the flow.
     */
    fun start(projectId: UUID, userId: UUID, returnUrl: String): String {
        val (client, opts) = clientAndOptions(projectId)

        // Reuse the stored account: a second call must not mint a second
        // Stripe account for the same user.
        val existing = accounts?.find(projectId, userId)
        val accountId = existing?.stripeAccountId ?: createAccount(client, opts, projectId, userId)

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
            throw PaymentError.InvalidState(
                "Stripe refused to mint the onboarding link: ${e.stripeError?.message ?: e.message}",
            )
        }

        if (existing == null) {
            accounts?.save(projectId, userId, accountId, url)
        }
        return url
    }

    private fun createAccount(
        client: StripeClient,
        opts: RequestOptions,
        projectId: UUID,
        userId: UUID,
    ): String = try {
        // Express: Stripe hosts the identity/bank collection. The metadata
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
        throw PaymentError.InvalidState(
            "Stripe refused to create the connected account: ${e.stripeError?.message ?: e.message}",
        )
    }
}
