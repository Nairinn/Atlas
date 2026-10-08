package io.atlas.payments

import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.PaymentsService
import io.atlas.payments.core.PayoutAccount
import io.atlas.payments.core.PayoutAccountSource
import io.atlas.payments.core.SettlementApplier
import io.atlas.payments.core.StripeWebhookHandler
import io.atlas.payments.core.TxStatus
import io.atlas.payments.fakes.DirectTransactionRunner
import io.atlas.payments.fakes.InMemoryOutbox
import io.atlas.payments.fakes.InMemoryTransactionRepository
import io.atlas.payments.fakes.InMemoryWalletRepository
import java.util.UUID
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

/**
 * The Stripe webhook handler against real-shaped fixture payloads, plus
 * the transition races the CAS exists for.
 */
class StripeWebhookHandlerTest {

    private val project = UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val rider = UUID.randomUUID()
    private val driver = UUID.randomUUID()
    private val wallets = InMemoryWalletRepository()
    private val transactions = InMemoryTransactionRepository()
    private val outbox = InMemoryOutbox()
    private val payoutAccounts = RecordingPayoutSource()

    private val applier = SettlementApplier(
        wallets = wallets,
        transactions = transactions,
        runner = DirectTransactionRunner(),
        outbox = outbox,
        fareTopic = "atlas.fare.events",
    )

    private val handler = StripeWebhookHandler(
        projectId = project,
        transactions = transactions,
        applier = applier,
        payoutAccounts = payoutAccounts,
    )

    private val service = PaymentsService(
        wallets = wallets,
        transactions = transactions,
        outbox = outbox,
        runner = DirectTransactionRunner(),
        provider = io.atlas.payments.core.FakePaymentProvider(),
        fareTopic = "atlas.fare.events",
    )

    private class RecordingPayoutSource : PayoutAccountSource {
        val enabled = mutableListOf<Pair<UUID, Boolean>>()
        var account: PayoutAccount? = null

        override fun accountFor(projectId: UUID, userId: UUID): PayoutAccount? = account

        override fun setPayoutsEnabled(projectId: UUID, userId: UUID, stripeAccountId: String, enabled: Boolean) {
            this.enabled += userId to enabled
        }
    }

    private fun pendingTxWithRef(providerRef: String): UUID {
        // Wallet-funded (fake provider, no destination): give the rider a
        // balance so settle's debit check passes.
        service.deposit(project, rider.toString(), 10_000, "dep-${UUID.randomUUID()}")
        val result = service.initiate(
            project, rider.toString(), driver.toString(), 2_000, "key-${UUID.randomUUID()}", "",
        )
        transactions.overrideProviderRef(result.transactionId, providerRef)
        return result.transactionId
    }

    // --- payment_intent.succeeded ------------------------------------------

    @Test
    fun `payment_intent succeeded settles a pending transaction`() {
        val id = pendingTxWithRef("pi_test_1")
        val body = """
            {"id": "evt_1", "type": "payment_intent.succeeded",
             "data": {"object": {"id": "pi_test_1", "status": "succeeded"}}}
        """.trimIndent()

        val result = handler.handle("payment_intent.succeeded", body)

        assertTrue(result is StripeWebhookHandler.Result.Applied)
        assertEquals(TxStatus.SETTLED, transactions.findById(project, id)!!.status)
        assertEquals(2_000, wallets.findByUser(project, driver)!!.balanceCents)
    }

    @Test
    fun `a duplicate succeeded delivery is ignored and moves nothing twice`() {
        val id = pendingTxWithRef("pi_test_2")
        val body = """{"id":"evt_2","type":"payment_intent.succeeded","data":{"object":{"id":"pi_test_2"}}}"""

        handler.handle("payment_intent.succeeded", body)
        val second = handler.handle("payment_intent.succeeded", body)

        assertTrue(second is StripeWebhookHandler.Result.Ignored)
        assertEquals(2_000, wallets.findByUser(project, driver)!!.balanceCents)
    }

    @Test
    fun `amount_capturable_updated does not settle`() {
        val id = pendingTxWithRef("pi_test_3")
        val body = """{"id":"evt_3","type":"payment_intent.amount_capturable_updated","data":{"object":{"id":"pi_test_3"}}}"""

        val result = handler.handle("payment_intent.amount_capturable_updated", body)

        assertTrue(result is StripeWebhookHandler.Result.Ignored)
        assertEquals(TxStatus.PENDING, transactions.findById(project, id)!!.status)
        assertEquals(0, wallets.findByUser(project, driver)!!.balanceCents)
    }

    // --- charge.refunded ----------------------------------------------------

    @Test
    fun `a full charge refund reverses a settled transaction`() {
        val id = pendingTxWithRef("pi_test_4")
        service.settle(project, id.toString())
        val body = """
            {"id": "evt_4", "type": "charge.refunded",
             "data": {"object": {"id": "ch_1", "payment_intent": "pi_test_4",
                                 "amount": 2000, "amount_refunded": 2000, "status": "succeeded"}}}
        """.trimIndent()

        val result = handler.handle("charge.refunded", body)

        assertTrue(result is StripeWebhookHandler.Result.Applied)
        assertEquals(TxStatus.REFUNDED, transactions.findById(project, id)!!.status)
        assertEquals(0, wallets.findByUser(project, driver)!!.balanceCents)
    }

    @Test
    fun `a partial refund is recorded but reverses nothing`() {
        val id = pendingTxWithRef("pi_test_5")
        service.settle(project, id.toString())
        val body = """
            {"id": "evt_5", "type": "charge.refunded",
             "data": {"object": {"id": "ch_2", "payment_intent": "pi_test_5",
                                 "amount": 2000, "amount_refunded": 500, "status": "succeeded"}}}
        """.trimIndent()

        val result = handler.handle("charge.refunded", body)

        assertTrue(result is StripeWebhookHandler.Result.Ignored, "partial refunds are not applied")
        assertEquals(TxStatus.SETTLED, transactions.findById(project, id)!!.status)
        assertEquals(2_000, wallets.findByUser(project, driver)!!.balanceCents)
    }

    // --- account.updated ------------------------------------------------------

    @Test
    fun `account updated with payouts_enabled true records the user`() {
        val userId = UUID.randomUUID()
        val body = """
            {"id": "acct_1", "type": "account.updated",
             "data": {"object": {"id": "acct_1", "payouts_enabled": true,
                                  "metadata": {"atlas_user_id": "$userId",
                                               "atlas_project_id": "$project"}}}}
        """.trimIndent()

        val result = handler.handle("account.updated", body)

        assertTrue(result is StripeWebhookHandler.Result.Applied)
        assertEquals(userId, payoutAccounts.enabled.single().first)
        assertEquals(true, payoutAccounts.enabled.single().second)
    }

    @Test
    fun `account updated with payouts_enabled false records false`() {
        val userId = UUID.randomUUID()
        val body = """
            {"id": "acct_2", "data": {"object": {"id": "acct_2",
                "payouts_enabled": false,
                "metadata": {"atlas_user_id": "$userId"}}}}
        """.trimIndent()

        val result = handler.handle("account.updated", body)

        assertTrue(result is StripeWebhookHandler.Result.Applied)
        assertEquals(false, payoutAccounts.enabled.single().second)
    }

    @Test
    fun `account updated without atlas metadata is ignored`() {
        val body = """{"id":"acct_3","data":{"object":{"id":"acct_3","payouts_enabled":true}}}"""

        val result = handler.handle("account.updated", body)

        assertTrue(result is StripeWebhookHandler.Result.Ignored)
    }

    // --- unknown and malformed ------------------------------------------------

    @Test
    fun `an unknown event type is ignored`() {
        val result = handler.handle("balance.available", """{"id":"evt_9"}""")
        assertTrue(result is StripeWebhookHandler.Result.Ignored)
    }

    @Test
    fun `succeeded without a pi id is malformed`() {
        val result = handler.handle("payment_intent.succeeded", """{"id":"evt_10","data":{"object":{"id":"ch_x"}}}""")
        assertTrue(result is StripeWebhookHandler.Result.Malformed)
    }

    // --- transition races ------------------------------------------------------

    @Test
    fun `two concurrent settles move the money exactly once`() {
        val id = pendingTxWithRef("pi_test_11")
        val tx = transactions.findById(project, id)!!

        // Two appliers race on the same record (the in-memory repo is
        // synchronized, so "concurrent" is forced by calling twice on the
        // same stale TxRecord — exactly the race the CAS guards against).
        val first = applier.applySettlement(tx)
        val second = applier.applySettlement(tx)

        assertTrue(first, "the first writer wins")
        assertTrue(!second, "the second writer must lose")
        assertEquals(2_000, wallets.findByUser(project, driver)!!.balanceCents)
        assertEquals(
            1,
            outbox.all().count {
                val e = atlas.events.FareEvent.parseFrom(it.payload)
                e.transactionId == id.toString() &&
                    e.eventType == atlas.events.FareEvent.EventType.TRANSACTION_SETTLED
            },
            "exactly one settled event",
        )
    }

    @Test
    fun `two concurrent refunds reverse the money exactly once`() {
        val id = pendingTxWithRef("pi_test_12")
        service.settle(project, id.toString())
        val tx = transactions.findById(project, id)!!

        val first = applier.applyRefund(tx)
        val second = applier.applyRefund(tx)

        assertTrue(first)
        assertTrue(!second)
        assertEquals(0, wallets.findByUser(project, driver)!!.balanceCents)
    }
}
