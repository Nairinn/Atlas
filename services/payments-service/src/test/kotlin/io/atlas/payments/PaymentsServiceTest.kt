package io.atlas.payments

import atlas.events.FareEvent
import io.atlas.payments.core.FakePaymentProvider
import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.PaymentsService
import io.atlas.payments.core.TxStatus
import io.atlas.payments.fakes.DirectTransactionRunner
import io.atlas.payments.fakes.InMemoryOutbox
import io.atlas.payments.fakes.InMemoryTransactionRepository
import io.atlas.payments.fakes.InMemoryWalletRepository
import java.util.UUID
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertNotEquals
import kotlin.test.assertNull
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class PaymentsServiceTest {

    // Two tenants, so the scoping can be asserted rather than assumed. A
    // suite that only ever used one project would pass just as happily
    // against repositories that ignored projectId entirely.
    private val projectA: UUID = UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val projectB: UUID = UUID.fromString("22222222-2222-2222-2222-222222222222")
    private val wallets = InMemoryWalletRepository()
    private val transactions = InMemoryTransactionRepository()
    private val outbox = InMemoryOutbox()
    private val topic = "atlas.fare.events"

    private val service = PaymentsService(
        wallets = wallets,
        transactions = transactions,
        outbox = outbox,
        runner = DirectTransactionRunner(),
        provider = FakePaymentProvider(),
        fareTopic = topic,
    )

    private val rider = UUID.randomUUID().toString()
    private val driver = UUID.randomUUID().toString()
    private val ride = UUID.randomUUID().toString()

    private fun lastEventType(): FareEvent.EventType =
        FareEvent.parseFrom(outbox.all().last().payload).eventType

    // --- initiate ---------------------------------------------------------

    @Test
    fun `initiate creates a pending transaction and a RIDE_ACCEPTED outbox event`() {
        val result = service.initiate(projectA, rider, driver, 1500, "key-1", ride)

        assertEquals(TxStatus.PENDING, result.status)
        val tx = transactions.findById(projectA, result.transactionId)!!
        assertEquals(1500, tx.amountCents)
        assertEquals(TxStatus.PENDING, tx.status)
        assertTrue(tx.providerRef!!.startsWith("fake_"))
        assertEquals(1, outbox.all().size)
        assertEquals(FareEvent.EventType.RIDE_ACCEPTED, lastEventType())
    }

    @Test
    fun `initiate is idempotent for the same key and args`() {
        val first = service.initiate(projectA, rider, driver, 1500, "key-1", ride)
        val second = service.initiate(projectA, rider, driver, 1500, "key-1", ride)

        assertEquals(first.transactionId, second.transactionId)
        assertEquals(1, outbox.all().size) // replay does not enqueue again
    }

    @Test
    fun `initiate rejects a reused key with different args`() {
        service.initiate(projectA, rider, driver, 1500, "key-1", ride)
        assertFailsWith<PaymentError.IdempotencyConflict> {
            service.initiate(projectA, rider, driver, 9999, "key-1", ride)
        }
    }

    @Test
    fun `initiate rejects a non-positive amount`() {
        assertFailsWith<PaymentError.InvalidAmount> {
            service.initiate(projectA, rider, driver, 0, "key-1", ride)
        }
    }

    @Test
    fun `initiate rejects identical payer and payee`() {
        assertFailsWith<PaymentError.InvalidArgument> {
            service.initiate(projectA, rider, rider, 1500, "key-1", ride)
        }
    }

    @Test
    fun `initiate rejects a non-uuid user id`() {
        assertFailsWith<PaymentError.InvalidArgument> {
            service.initiate(projectA, "not-a-uuid", driver, 1500, "key-1", ride)
        }
    }

    // --- settle -----------------------------------------------------------

    @Test
    fun `settle moves balances and emits TRANSACTION_SETTLED`() {
        val tx = transactions.findById(projectA, service.initiate(projectA, rider, driver, 1500, "key-1", ride).transactionId)!!
        // Fund the payer so the capture has something to move.
        wallets.adjustBalance(projectA, tx.fromWallet!!, 5000)

        val result = service.settle(projectA, tx.id.toString())

        assertTrue(result.success)
        assertEquals(TxStatus.SETTLED, result.status)
        assertEquals(3500, wallets.findById(projectA, tx.fromWallet!!)!!.balanceCents)
        assertEquals(1500, wallets.findById(projectA, tx.toWallet!!)!!.balanceCents)
        assertEquals(FareEvent.EventType.TRANSACTION_SETTLED, lastEventType())
    }

    @Test
    fun `settle is idempotent`() {
        val tx = transactions.findById(projectA, service.initiate(projectA, rider, driver, 1500, "key-1", ride).transactionId)!!
        wallets.adjustBalance(projectA, tx.fromWallet!!, 5000)
        service.settle(projectA, tx.id.toString())
        val outboxAfterFirst = outbox.all().size

        val again = service.settle(projectA, tx.id.toString())

        assertTrue(again.success)
        assertEquals(outboxAfterFirst, outbox.all().size) // no second event
    }

    @Test
    fun `settle rejects insufficient funds`() {
        val tx = transactions.findById(projectA, service.initiate(projectA, rider, driver, 1500, "key-1", ride).transactionId)!!
        // Payer wallet left at zero balance.
        assertFailsWith<PaymentError.InsufficientFunds> {
            service.settle(projectA, tx.id.toString())
        }
    }

    @Test
    fun `settle of an unknown transaction is NOT_FOUND`() {
        assertFailsWith<PaymentError.TransactionNotFound> {
            service.settle(projectA, UUID.randomUUID().toString())
        }
    }

    // --- refund -----------------------------------------------------------

    @Test
    fun `refund reverses a settled transaction and emits TRANSACTION_REFUNDED`() {
        val tx = transactions.findById(projectA, service.initiate(projectA, rider, driver, 1500, "key-1", ride).transactionId)!!
        wallets.adjustBalance(projectA, tx.fromWallet!!, 5000)
        service.settle(projectA, tx.id.toString())

        val result = service.refund(projectA, tx.id.toString())

        assertTrue(result.success)
        assertEquals(TxStatus.REFUNDED, transactions.findById(projectA, tx.id)!!.status)
        assertEquals(5000, wallets.findById(projectA, tx.fromWallet!!)!!.balanceCents) // refunded back
        assertEquals(0, wallets.findById(projectA, tx.toWallet!!)!!.balanceCents)
        assertEquals(FareEvent.EventType.TRANSACTION_REFUNDED, lastEventType())
    }

    @Test
    fun `refund of a pending transaction cancels it instead`() {
        val initiate = service.initiate(projectA, rider, driver, 1500, "key-1", ride)
        val tx = transactions.findById(projectA, initiate.transactionId)!!

        val result = service.refund(projectA, tx.id.toString())

        assertTrue(result.success)
        assertEquals(TxStatus.CANCELLED, transactions.findById(projectA, tx.id)!!.status)
        // Nothing was captured, so nothing moves and nothing is owed.
        assertEquals(0, wallets.findByUser(projectA, UUID.fromString(rider))!!.balanceCents)
        assertEquals(0, wallets.findByUser(projectA, UUID.fromString(driver))!!.balanceCents)
    }

    // --- wallet -----------------------------------------------------------

    @Test
    fun `walletBalance returns zero for an unknown user`() {
        val balance = service.walletBalance(projectA, UUID.randomUUID().toString())
        assertEquals(0, balance.balanceCents)
        assertEquals("USD", balance.currency)
    }

    @Test
    fun `walletBalance reflects a credited wallet`() {
        val tx = transactions.findById(projectA, service.initiate(projectA, rider, driver, 1500, "key-1", ride).transactionId)!!
        wallets.adjustBalance(projectA, tx.fromWallet!!, 5000)
        val balance = service.walletBalance(projectA, rider)
        assertEquals(5000, balance.balanceCents)
    }

    // --- tenancy ----------------------------------------------------------
    //
    // Everything above uses one project, so all of it would pass against
    // repositories that ignored projectId entirely. These would not.

    /**
     * The dangerous one.
     *
     * Idempotency keys are chosen by the caller — "order-1" is the string
     * every integration reaches for first — and while the unique index was
     * global, the second tenant to use it did not get an error. It got the
     * FIRST tenant's transaction handed back as a successful idempotent
     * replay: someone else's payment, reported as a success.
     */
    @Test
    fun `the same idempotency key in two projects is two transactions`() {
        val a = service.initiate(projectA, rider, driver, 1500, "order-1", ride)
        val b = service.initiate(projectB, rider, driver, 9999, "order-1", ride)

        assertNotEquals(
            a.transactionId,
            b.transactionId,
            "each project must get its own transaction for the same key",
        )
        assertEquals(1500, transactions.findById(projectA, a.transactionId)!!.amountCents)
        assertEquals(9999, transactions.findById(projectB, b.transactionId)!!.amountCents)
    }

    /** Scoping the key must not weaken idempotency inside one project. */
    @Test
    fun `the same idempotency key within one project is still one transaction`() {
        val first = service.initiate(projectA, rider, driver, 1500, "order-1", ride)
        val second = service.initiate(projectA, rider, driver, 1500, "order-1", ride)
        assertEquals(first.transactionId, second.transactionId)
    }

    /**
     * Wallets are per project too. The same user id in two projects is two
     * people with two balances — which is the only coherent reading once
     * users themselves are scoped.
     */
    @Test
    fun `wallets do not leak between projects`() {
        val tx = transactions.findById(
            projectA,
            service.initiate(projectA, rider, driver, 1500, "key-1", ride).transactionId,
        )!!
        wallets.adjustBalance(projectA, tx.fromWallet!!, 5000)

        assertEquals(5000, service.walletBalance(projectA, rider).balanceCents)
        assertEquals(
            0,
            service.walletBalance(projectB, rider).balanceCents,
            "the same user id in another project must have its own wallet",
        )
    }

    /** A transaction id from one project must be invisible to another. */
    @Test
    fun `a transaction cannot be settled from another project`() {
        val tx = service.initiate(projectA, rider, driver, 1500, "key-1", ride)
        assertNull(
            transactions.findById(projectB, tx.transactionId),
            "another project must not be able to read this transaction",
        )
        assertFailsWith<PaymentError.TransactionNotFound> {
            service.settle(projectB, tx.transactionId.toString())
        }
    }

    /**
     * Paying a user who is not in your project must fail as the caller's
     * mistake, not as an internal error.
     *
     * `to_user_id` is the one identifier a caller supplies rather than
     * having derived from their credentials — the whole point is paying
     * somebody else — and nothing checked it belonged to their project.
     * Migration 0080 makes the database refuse it; this asserts the fake
     * refuses it the same way, because a double that is more permissive
     * than the schema hides exactly this class of bug.
     */
    @Test
    fun `paying a user outside the project is refused`() {
        val stranger = UUID.randomUUID().toString()
        wallets.registerMember(projectA, UUID.fromString(rider))
        // `stranger` is deliberately not registered in projectA.

        assertFailsWith<PaymentError.UnknownUser> {
            service.initiate(projectA, rider, stranger, 1500, "key-x", ride)
        }
    }

    @AfterTest
    fun noOp() = Unit
}
