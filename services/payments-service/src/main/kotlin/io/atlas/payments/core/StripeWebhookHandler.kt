package io.atlas.payments.core

import java.util.UUID

/**
 * Handles verified Stripe webhook events for one project.
 *
 * The endpoint verifies the signature and hands the project and RAW body
 * here; this class does the acting. Anything it cannot act on returns
 * [Result.Ignored] rather than throwing, because a 500 to Stripe means
 * "retry this event forever" — and an event Atlas cannot act on is, from
 * Stripe's point of view, successfully delivered.
 *
 * Only `payment_intent.succeeded` settles. `amount_capturable_updated`
 * means "authorized, awaiting capture", not captured: under the
 * client-side-confirmation flow the app's client completes the payment and
 * Stripe's terminal word is `succeeded`. `charge.refunded` reverses only
 * a FULL refund; a partial refund is recorded but reverses nothing,
 * because the internal ledger does not model half a fare.
 */
class StripeWebhookHandler(
    private val projectId: UUID,
    private val transactions: TransactionRepository,
    private val applier: SettlementApplier,
    private val payoutAccounts: PayoutAccountSource,
) {
    sealed class Result {
        /** State changed; balances moved. */
        data class Applied(val what: String) : Result()

        /** Known event, nothing to do — already processed, or not ours. */
        data class Ignored(val why: String) : Result()

        /** We could not parse what Stripe sent. Logged, never a 5xx. */
        data class Malformed(val why: String) : Result()
    }

    fun handle(eventType: String, payloadJson: String): Result =
        when (eventType) {
            "payment_intent.succeeded" -> {
                val intentId = extractString(payloadJson, """"id"\s*:\s*"(pi_[^"]+)"""")
                    ?: return Result.Malformed("payment_intent.succeeded without a pi_ id")
                settleByProviderRef(intentId)
            }
            "payment_intent.amount_capturable_updated" ->
                // Authorized, not captured. The client still has to confirm.
                Result.Ignored("amount_capturable_updated: awaiting client confirmation")
            "charge.refunded" -> refundFromCharge(payloadJson)
            "account.updated" -> updatePayoutAccount(payloadJson)
            else -> Result.Ignored("event type $eventType is not acted on")
        }

    private fun settleByProviderRef(intentId: String): Result {
        val tx = transactions.findByProviderRef(projectId, intentId)
            ?: return Result.Ignored("no transaction for $intentId in this project")
        return when (tx.status) {
            TxStatus.PENDING ->
                if (applier.applySettlement(tx)) Result.Applied("settled ${tx.id}")
                else Result.Ignored("${tx.id} settled by a concurrent delivery")
            TxStatus.SETTLED -> Result.Ignored("${tx.id} already settled (duplicate delivery)")
            else -> Result.Ignored("${tx.id} is ${tx.status}; webhook settlement does not apply")
        }
    }

    private fun refundFromCharge(payloadJson: String): Result {
        val intentId = extractString(payloadJson, """"payment_intent"\s*:\s*"(pi_[^"]+)"""")
            ?: return Result.Malformed("charge.refunded without a payment_intent")
        val amountRefunded = extractLong(payloadJson, """"amount_refunded"\s*:\s*(\d+)""")
            ?: return Result.Malformed("charge.refunded without amount_refunded")
        val amount = extractLong(payloadJson, """"amount"\s*:\s*(\d+)""")
            ?: return Result.Malformed("charge.refunded without amount")

        if (amountRefunded < amount) {
            // Partial refund: the internal ledger models whole fares, so
            // there is nothing sane to reverse. Recorded (the audit trail
            // has the event) rather than applied.
            return Result.Ignored(
                "partial refund of $intentId ($amountRefunded of $amount cents) recorded, not applied",
            )
        }

        val tx = transactions.findByProviderRef(projectId, intentId)
            ?: return Result.Ignored("no transaction for $intentId in this project")
        return when (tx.status) {
            TxStatus.SETTLED ->
                if (applier.applyRefund(tx)) Result.Applied("refunded ${tx.id}")
                else Result.Ignored("${tx.id} refunded by a concurrent delivery")
            TxStatus.REFUNDED -> Result.Ignored("${tx.id} already refunded (duplicate delivery)")
            else -> Result.Ignored("${tx.id} is ${tx.status}; only a settled transaction can be refunded")
        }
    }

    private fun updatePayoutAccount(payloadJson: String): Result {
        val accountId = extractString(payloadJson, """"id"\s*:\s*"(acct_[^"]+)"""")
            ?: return Result.Malformed("account.updated without an account id")
        val userId = extractString(payloadJson, """"atlas_user_id"\s*:\s*"([^"]+)"""")
            ?: return Result.Ignored("account $accountId carries no atlas_user_id; not ours")
        val enabled = Regex(""""payouts_enabled"\s*:\s*true""").containsMatchIn(payloadJson)
        val uuid = parse(userId)
        payoutAccounts.setPayoutsEnabled(projectId, uuid, accountId, enabled)
        return Result.Applied("account $accountId payouts_enabled=$enabled")
    }

    private fun parse(s: String): UUID =
        try {
            UUID.fromString(s)
        } catch (e: IllegalArgumentException) {
            throw IllegalStateException("atlas_user_id metadata was not a UUID: $s")
        }

    private fun extractString(json: String, pattern: String): String? =
        Regex(pattern).find(json)?.groupValues?.getOrNull(1)

    private fun extractLong(json: String, pattern: String): Long? =
        Regex(pattern).find(json)?.groupValues?.getOrNull(1)?.toLongOrNull()
}
