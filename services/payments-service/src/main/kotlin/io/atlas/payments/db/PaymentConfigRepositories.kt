package io.atlas.payments.db

import io.atlas.payments.core.PayoutAccount
import io.atlas.payments.core.PayoutAccountSource
import io.atlas.payments.core.PaymentConfigSource
import io.atlas.payments.core.ProjectPaymentConfig
import io.atlas.payments.crypto.ConfigCipher
import io.atlas.payments.stripe.StripeOnboarding
import org.jetbrains.exposed.sql.ResultRow
import org.jetbrains.exposed.sql.and
import org.jetbrains.exposed.sql.insert
import org.jetbrains.exposed.sql.javatime.timestamp
import org.jetbrains.exposed.sql.selectAll
import org.jetbrains.exposed.sql.transactions.transaction
import org.jetbrains.exposed.sql.update
import java.util.UUID

/**
 * Exposed table for control.project_payment_config (migration 0090).
 *
 * Lives in the payments db package rather than a control-plane one
 * because the payments service is this row's only reader; the control
 * plane writes it (an operator connecting a Stripe account through the
 * dashboard), and the schema prefix is built into the name so one
 * connection addresses every Atlas schema.
 */
object ProjectPaymentConfigTable : org.jetbrains.exposed.sql.Table("control.project_payment_config") {
    val projectId = uuid("project_id")
    val secretKeyEnc = binary("secret_key_enc")
    val webhookSecretEnc = binary("webhook_secret_enc")
    val currency = text("currency")
    val provider = text("provider")
    val createdAt = timestamp("created_at")
    val updatedAt = timestamp("updated_at")

    override val primaryKey = PrimaryKey(projectId)
}

/** Exposed table for payments.stripe_accounts (migration 0090). */
object StripeAccounts : org.jetbrains.exposed.sql.Table("payments.stripe_accounts") {
    val id = uuid("id").autoGenerate()
    val projectId = uuid("project_id")
    val userId = uuid("user_id")
    val stripeAccountId = text("stripe_account_id")
    val payoutsEnabled = bool("payouts_enabled")
    val onboardingUrl = text("onboarding_url").nullable()
    val createdAt = timestamp("created_at")
    val updatedAt = timestamp("updated_at")

    override val primaryKey = PrimaryKey(id)

    init {
        // Matches the UNIQUE(project_id, user_id) constraint from 0090.
        uniqueIndex(projectId, userId)
    }
}

/**
 * Decryption is per ROW, not per query: the cipher is configured once at
 * startup from PAYMENT_CONFIG_ENC_KEY and only the ciphertext ever comes
 * back from Postgres.
 */
class ExposedPaymentConfigSource(
    private val cipher: ConfigCipher,
) : PaymentConfigSource {

    override fun configFor(projectId: UUID): ProjectPaymentConfig? = transaction {
        ProjectPaymentConfigTable
            .selectAll()
            .where { ProjectPaymentConfigTable.projectId eq projectId }
            .singleOrNull()
            ?.toConfig()
    }

    private fun ResultRow.toConfig() = ProjectPaymentConfig(
        projectId = this[ProjectPaymentConfigTable.projectId],
        provider = this[ProjectPaymentConfigTable.provider],
        secretKey = cipher.decrypt(this[ProjectPaymentConfigTable.secretKeyEnc]),
        webhookSecret = cipher.decrypt(this[ProjectPaymentConfigTable.webhookSecretEnc]),
        currency = this[ProjectPaymentConfigTable.currency],
    )
}

class ExposedPayoutAccountSource : PayoutAccountSource, StripeOnboarding.AccountStore {

    override fun accountFor(projectId: UUID, userId: UUID): PayoutAccount? = transaction {
        StripeAccounts
            .selectAll()
            .where { (StripeAccounts.projectId eq projectId) and (StripeAccounts.userId eq userId) }
            .singleOrNull()
            ?.let { PayoutAccount(it[StripeAccounts.stripeAccountId], it[StripeAccounts.payoutsEnabled]) }
    }

    override fun setPayoutsEnabled(projectId: UUID, userId: UUID, stripeAccountId: String, enabled: Boolean) {
        transaction {
            StripeAccounts.update({
                (StripeAccounts.projectId eq projectId) and
                    (StripeAccounts.userId eq userId) and
                    (StripeAccounts.stripeAccountId eq stripeAccountId)
            }) {
                it[payoutsEnabled] = enabled
                it[updatedAt] = java.time.Instant.now()
            }
        }
    }

    /** StripeOnboarding.AccountStore: same lookup as [accountFor]. */
    override fun find(projectId: UUID, userId: UUID): PayoutAccount? = accountFor(projectId, userId)

    /**
     * Insert, or update the stored account id if it drifted (the row must
     * always name the account a link was actually minted for).
     */
    override fun save(projectId: UUID, userId: UUID, stripeAccountId: String, onboardingUrl: String) {
        transaction {
            val existingId = StripeAccounts
                .selectAll()
                .where { (StripeAccounts.projectId eq projectId) and (StripeAccounts.userId eq userId) }
                .singleOrNull()
                ?.get(StripeAccounts.id)

            if (existingId == null) {
                StripeAccounts.insert {
                    it[StripeAccounts.projectId] = projectId
                    it[StripeAccounts.userId] = userId
                    it[StripeAccounts.stripeAccountId] = stripeAccountId
                    it[payoutsEnabled] = false
                    it[StripeAccounts.onboardingUrl] = onboardingUrl
                    it[createdAt] = java.time.Instant.now()
                    it[updatedAt] = java.time.Instant.now()
                }
            } else {
                StripeAccounts.update({
                    (StripeAccounts.projectId eq projectId) and (StripeAccounts.userId eq userId)
                }) {
                    it[StripeAccounts.stripeAccountId] = stripeAccountId
                    it[StripeAccounts.onboardingUrl] = onboardingUrl
                    it[updatedAt] = java.time.Instant.now()
                }
            }
        }
    }
}
