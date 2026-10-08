package io.atlas.payments.core

import java.util.UUID

/**
 * Per-project processor configuration. Production reads it from
 * `control.project_payment_config` (see io.atlas.payments.db); tests
 * provide it in memory.
 *
 * This is the port that makes Connect work: the processor credentials are
 * the TENANT's, not Atlas's, so nothing about a charge can be resolved
 * until the project is known. A provider constructed without a source
 * like this would have exactly one credential — the mistake the per-
 * project method signatures exist to make unrepresentable.
 */
data class ProjectPaymentConfig(
    val projectId: UUID,
    /** 'stripe' or 'fake'. Decided per project, not per deployment. */
    val provider: String,
    /** Plaintext processor secret key; only ever in memory. */
    val secretKey: String,
    /** Per-endpoint webhook signing secret. */
    val webhookSecret: String,
    /** Three-letter ISO currency the project charges in. */
    val currency: String,
)

/**
 * Resolves a project's payment configuration, or null when the project
 * has not connected a processor.
 *
 * Implementations may cache: the row changes only when an operator
 * rotates a credential, and the lookup sits on the authorize path of
 * every charge.
 */
interface PaymentConfigSource {
    fun configFor(projectId: UUID): ProjectPaymentConfig?
}

/**
 * The payee side of Connect: the processor account that pays a given
 * user, when they have finished onboarding one.
 */
interface PayoutAccountSource {
    /**
     * The user's processor account id, or null when they have none.
     * `payoutsEnabled` false means onboarding started but Stripe has
     * not finished verifying them — money cannot be sent there yet.
     */
    fun accountFor(projectId: UUID, userId: UUID): PayoutAccount?

    /** Flipped by the account.updated webhook. */
    fun setPayoutsEnabled(projectId: UUID, userId: UUID, stripeAccountId: String, enabled: Boolean)
}

data class PayoutAccount(
    val stripeAccountId: String,
    val payoutsEnabled: Boolean,
)

/**
 * A [PaymentConfigSource] with no database: the deployment every project
 * of which runs the default provider, and tests that want to pin a
 * project's config without Postgres.
 */
class StaticPaymentConfigSource(
    private val configs: Map<UUID, ProjectPaymentConfig>,
) : PaymentConfigSource {
    override fun configFor(projectId: UUID): ProjectPaymentConfig? = configs[projectId]
}

/**
 * Routes each project to the processor its configuration names.
 *
 * The name is the contract: a project with provider='stripe' talks to
 * the Stripe adapter, everything else talks to [default] — which is
 * itself the PAYMENT_PROVIDER default and fails loudly at startup when it
 * is an unknown name. This is the object that makes "one deployment
 * serves a test tenant on the fake and a paying tenant on Stripe" a
 * property of configuration rather than of code paths.
 */
class PerProjectPaymentProvider(
    private val default: PaymentProvider,
    private val configs: PaymentConfigSource,
    /** The Stripe adapter; null when the deployment has no Stripe at all. */
    private val stripe: PaymentProvider?,
) : PaymentProvider {
    override val name: String = "per-project"

    private fun forProject(projectId: UUID): PaymentProvider {
        val config = configs.configFor(projectId) ?: return default
        return when (config.provider.trim().lowercase()) {
            "stripe" -> stripe
                ?: throw IllegalStateException(
                    "project $projectId is configured for stripe but the deployment has no " +
                        "Stripe adapter (check PAYMENT_CONFIG_ENC_KEY)",
                )
            "fake" -> default
            else -> default
        }
    }

    override fun authorize(request: ChargeRequest, idempotencyKey: String): ProviderResult =
        forProject(request.projectId).authorize(request, idempotencyKey)

    override fun capture(projectId: UUID, providerRef: String): ProviderResult =
        forProject(projectId).capture(projectId, providerRef)

    override fun cancel(projectId: UUID, providerRef: String): ProviderResult =
        forProject(projectId).cancel(projectId, providerRef)

    override fun refund(projectId: UUID, providerRef: String): ProviderResult =
        forProject(projectId).refund(projectId, providerRef)

    override fun clientSecret(projectId: UUID, providerRef: String): String? =
        forProject(projectId).clientSecret(projectId, providerRef)

    override fun lookup(projectId: UUID, providerRef: String): ProviderStatus =
        forProject(projectId).lookup(projectId, providerRef)

    override fun verifyWebhook(projectId: UUID, payload: String, signature: String?): Boolean =
        forProject(projectId).verifyWebhook(projectId, payload, signature)
}
