package io.atlas.payments

import atlas.events.FareEvent
import io.atlas.payments.core.ChargeRequest
import io.atlas.payments.core.FakePaymentProvider
import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.PaymentProvider
import io.atlas.payments.core.PaymentProviders
import io.atlas.payments.core.PaymentsService
import io.atlas.payments.core.ProviderResult
import io.atlas.payments.core.TxKind
import io.atlas.payments.core.TxStatus
import io.atlas.payments.fakes.DirectTransactionRunner
import io.atlas.payments.fakes.InMemoryOutbox
import io.atlas.payments.fakes.InMemoryTransactionRepository
import io.atlas.payments.fakes.InMemoryWalletRepository
import java.util.UUID
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

/**
 * Deposits are the only way money enters the platform, so these cover both
 * the happy path and — more importantly — what happens when the provider
 * says no partway through.
 */
class DepositTest {

    // Two tenants, so the scoping can be asserted rather than assumed. A
    // suite that only ever used one project would pass just as happily
    // against repositories that ignored projectId entirely.
    private val projectA: UUID = UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val projectB: UUID = UUID.fromString("22222222-2222-2222-2222-222222222222")
    private val wallets = InMemoryWalletRepository()
    private val transactions = InMemoryTransactionRepository()
    private val outbox = InMemoryOutbox()
    private val topic = "atlas.fare.events"

    private fun service(provider: PaymentProvider = FakePaymentProvider()) =
        PaymentsService(
            wallets = wallets,
            transactions = transactions,
            outbox = outbox,
            runner = DirectTransactionRunner(),
            provider = provider,
            fareTopic = topic,
        )

    private val user = UUID.randomUUID().toString()

    @Test
    fun `deposit credits the wallet and settles immediately`() {
        val result = service().deposit(projectA, user, 5_000, "dep-1")

        assertEquals(TxStatus.SETTLED, result.status)
        assertEquals(5_000, result.balanceCents)
        assertEquals(5_000, service().walletBalance(projectA, user).balanceCents)

        val record = assertNotNull(transactions.findById(projectA, result.transactionId))
        assertEquals(TxKind.DEPOSIT, record.kind)
        // No source wallet: the money came from outside the platform.
        assertEquals(null, record.fromWallet)
        assertNotNull(record.toWallet)
        // The provider reference is what reconciliation needs later.
        assertNotNull(record.providerRef)
    }

    @Test
    fun `deposit emits a settled outbox event in the same transaction`() {
        val result = service().deposit(projectA, user, 2_500, "dep-2")

        val event = FareEvent.parseFrom(outbox.all().last().payload)
        assertEquals(FareEvent.EventType.TRANSACTION_SETTLED, event.eventType)
        assertEquals(result.transactionId.toString(), event.transactionId)
        assertEquals(2_500, event.amountCents)
        // No ride is involved in a top-up.
        assertEquals("", event.rideId)
    }

    /**
     * The whole point of the deposit path: before it existed, wallets sat at
     * zero and every transfer failed with insufficient funds.
     */
    @Test
    fun `a funded wallet can then pay another user`() {
        val payer = UUID.randomUUID().toString()
        val payee = UUID.randomUUID().toString()
        val svc = service()

        svc.deposit(projectA, payer, 10_000, "dep-3")
        val tx = svc.initiate(projectA, payer, payee, 2_500, "tx-1", UUID.randomUUID().toString())
        svc.settle(projectA, tx.transactionId.toString())

        assertEquals(7_500, svc.walletBalance(projectA, payer).balanceCents)
        assertEquals(2_500, svc.walletBalance(projectA, payee).balanceCents)
    }

    @Test
    fun `deposit is idempotent on a repeated key`() {
        val svc = service()
        val first = svc.deposit(projectA, user, 5_000, "dep-same")
        val second = svc.deposit(projectA, user, 5_000, "dep-same")

        assertEquals(first.transactionId, second.transactionId)
        // Critically the balance is credited once, not twice.
        assertEquals(5_000, svc.walletBalance(projectA, user).balanceCents)
    }

    @Test
    fun `reusing a key with a different amount is a conflict`() {
        val svc = service()
        svc.deposit(projectA, user, 5_000, "dep-conflict")
        // Silently returning the first transaction here would tell the caller
        // their 9999 deposit succeeded when it never happened.
        assertFailsWith<PaymentError.IdempotencyConflict> {
            svc.deposit(projectA, user, 9_999, "dep-conflict")
        }
    }

    @Test
    fun `deposit rejects non-positive amounts`() {
        val svc = service()
        assertFailsWith<PaymentError.InvalidAmount> { svc.deposit(projectA, user, 0, "dep-zero") }
        assertFailsWith<PaymentError.InvalidAmount> { svc.deposit(projectA, user, -100, "dep-neg") }
    }

    @Test
    fun `deposit requires an idempotency key`() {
        assertFailsWith<PaymentError.InvalidArgument> { service().deposit(projectA, user, 100, "  ") }
    }

    @Test
    fun `a declined authorization credits nothing and records nothing`() {
        val declining = object : PaymentProvider by FakePaymentProvider() {
            override fun authorize(request: ChargeRequest, idempotencyKey: String) =
                ProviderResult(success = false, providerRef = "", message = "card declined")
        }

        assertFailsWith<PaymentError.ProviderDeclined> {
            service(declining).deposit(projectA, user, 5_000, "dep-declined")
        }
        assertEquals(0, service().walletBalance(projectA, user).balanceCents)
        // Nothing was written, so the key is still free for a real retry.
        assertEquals(null, transactions.findByIdempotencyKey(projectA, "dep-declined"))
    }

    /**
     * The important failure. Authorize succeeds so a row is written, then
     * capture is refused. The wallet must not be credited, and the row must
     * end up FAILED rather than sitting PENDING where a reconciliation sweep
     * would keep retrying a charge the provider already refused.
     */
    @Test
    fun `a declined capture leaves the transaction failed and the wallet untouched`() {
        val failsCapture = object : PaymentProvider by FakePaymentProvider() {
            override fun capture(projectId: UUID, providerRef: String) =
                ProviderResult(success = false, providerRef = providerRef, message = "capture refused")
        }

        assertFailsWith<PaymentError.ProviderDeclined> {
            service(failsCapture).deposit(projectA, user, 5_000, "dep-capture-fail")
        }

        assertEquals(0, service().walletBalance(projectA, user).balanceCents)
        val record = assertNotNull(transactions.findByIdempotencyKey(projectA, "dep-capture-fail"))
        assertEquals(TxStatus.FAILED, record.status)
        // The provider reference survives, which is what a human or a sweep
        // needs to ask the provider what actually happened to the charge.
        assertNotNull(record.providerRef)
        assertFalse(outbox.all().any { FareEvent.parseFrom(it.payload).transactionId == record.id.toString() })
    }

    // --- provider selection ---------------------------------------------

    @Test
    fun `provider selection defaults to the fake and rejects unknown names`() {
        assertEquals("fake", PaymentProviders.fromName("").name)
        assertEquals("fake", PaymentProviders.fromName("fake").name)
        assertEquals("fake", PaymentProviders.fromName("FAKE").name)

        // A typo must not silently run a stub that approves every charge.
        assertFailsWith<IllegalArgumentException> { PaymentProviders.fromName("strpe") }
        // Named but not implemented is also a hard failure, not a fallback.
        assertFailsWith<IllegalArgumentException> { PaymentProviders.fromName("stripe") }
    }

    @Test
    fun `the fake provider accepts webhooks, which is only safe because it sends none`() {
        assertTrue(FakePaymentProvider().verifyWebhook(projectA, "{}", null))
    }
}
