package io.atlas.payments

import io.atlas.payments.core.ChargeRequest
import io.atlas.payments.core.PaymentProvider
import io.atlas.payments.core.ProviderResult
import io.atlas.payments.core.ProviderStatus
import io.atlas.payments.core.ReconciliationSweep
import io.atlas.payments.core.TxKind
import io.atlas.payments.core.TxStatus
import io.atlas.payments.core.SettlementApplier
import io.atlas.payments.fakes.InMemoryOutbox
import io.atlas.payments.fakes.InMemoryTransactionRepository
import io.atlas.payments.fakes.InMemoryWalletRepository
import io.atlas.payments.fakes.DirectTransactionRunner
import java.time.Clock
import java.time.Duration
import java.time.Instant
import java.time.ZoneOffset
import java.util.UUID
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * Reconciliation of transactions stuck in PENDING.
 *
 * A pending row means Atlas started a charge and never recorded the
 * outcome. The customer may have been charged with nothing to show for
 * it, or may not have been at all, and both look identical from inside.
 * The provider is the only source of truth — so these tests are mostly
 * about the sweep believing it, including when it says "I don't know".
 */
class ReconciliationTest {

    private val project = UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val now = Instant.parse("2026-08-19T12:00:00Z")
    private val clock = Clock.fixed(now, ZoneOffset.UTC)

    /** A provider whose answers the test dictates. */
    private class ScriptedProvider(
        private val answers: Map<String, ProviderStatus> = emptyMap(),
        private val throwOn: Set<String> = emptySet(),
    ) : PaymentProvider {
        override val name = "scripted"
        var lookups = 0
            private set

        override fun authorize(request: ChargeRequest, idempotencyKey: String) =
            ProviderResult(true, "ref_$idempotencyKey")

        override fun capture(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun refund(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun cancel(projectId: UUID, providerRef: String) = ProviderResult(true, providerRef)
        override fun clientSecret(projectId: UUID, providerRef: String): String? = null
        override fun verifyWebhook(projectId: UUID, payload: String, signature: String?) = true

        override fun lookup(projectId: UUID, providerRef: String): ProviderStatus {
            lookups++
            if (providerRef in throwOn) throw RuntimeException("provider unreachable")
            return answers[providerRef] ?: ProviderStatus.UNKNOWN
        }
    }

    private class Harness(
        val sweep: ReconciliationSweep,
        val transactions: InMemoryTransactionRepository,
        val wallets: InMemoryWalletRepository,
        val provider: ScriptedProvider,
    )

    private fun harness(provider: ScriptedProvider): Harness {
        val transactions = InMemoryTransactionRepository()
        val wallets = InMemoryWalletRepository()
        val outbox = InMemoryOutbox()
        val applier = SettlementApplier(
            wallets = wallets,
            transactions = transactions,
            runner = DirectTransactionRunner(),
            outbox = outbox,
            fareTopic = "atlas.fare.events",
            clock = clock,
        )
        return Harness(
            ReconciliationSweep(
                transactions = transactions,
                applier = applier,
                provider = provider,
                stuckAfter = Duration.ofMinutes(15),
                clock = clock,
            ),
            transactions, wallets, provider,
        )
    }

    /** A pending transaction created [age] ago, with a provider reference. */
    private fun stuckTransaction(
        h: Harness,
        age: Duration,
        providerRef: String? = "ref_1",
        amount: Long = 2_500,
    ): UUID {
        val from = h.wallets.getOrCreateByUser(project, UUID.randomUUID())
        val to = h.wallets.getOrCreateByUser(project, UUID.randomUUID())
        h.wallets.adjustBalance(project, from.id, 10_000)
        val record = h.transactions.insertPending(
            project,
            fromWallet = from.id,
            toWallet = to.id,
            amountCents = amount,
            idempotencyKey = UUID.randomUUID().toString(),
            rideId = null,
            providerRef = providerRef,
            argsHash = null,
            kind = TxKind.TRANSFER,
        )
        h.transactions.backdate(record.id, now.minus(age))
        return record.id
    }

    // --- the sweep resolving what it can ---------------------------------

    @Test
    fun `a charge the provider captured is settled, and the money moves`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.CAPTURED)))
        val id = stuckTransaction(h, Duration.ofHours(1))
        val tx = h.transactions.findById(project, id)!!
        val before = h.wallets.findById(project, tx.toWallet!!)!!.balanceCents

        val outcome = h.sweep.runOnce()

        assertEquals(1, outcome.settled)
        assertEquals(TxStatus.SETTLED, h.transactions.findById(project, id)!!.status)
        assertEquals(
            before + 2_500,
            h.wallets.findById(project, tx.toWallet!!)!!.balanceCents,
            "settling must apply the balance movement the original settle would have",
        )
    }

    @Test
    fun `a charge the provider declined is failed`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.FAILED)))
        val id = stuckTransaction(h, Duration.ofHours(1))

        assertEquals(1, h.sweep.runOnce().failed)
        assertEquals(TxStatus.FAILED, h.transactions.findById(project, id)!!.status)
    }

    /** A reference the provider has never heard of never charged anybody. */
    @Test
    fun `a charge the provider has no record of is failed`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.NOT_FOUND)))
        val id = stuckTransaction(h, Duration.ofHours(1))

        assertEquals(1, h.sweep.runOnce().failed)
        assertEquals(TxStatus.FAILED, h.transactions.findById(project, id)!!.status)
    }

    /**
     * A pending row with no provider reference never reached the provider,
     * so nothing was charged and failing it is safe — and it is the only
     * outcome that frees the idempotency key for a retry.
     */
    @Test
    fun `a transaction that never reached the provider is failed without asking`() {
        val h = harness(ScriptedProvider())
        val id = stuckTransaction(h, Duration.ofHours(1), providerRef = null)

        assertEquals(1, h.sweep.runOnce().failed)
        assertEquals(TxStatus.FAILED, h.transactions.findById(project, id)!!.status)
        assertEquals(0, h.provider.lookups, "there is nothing to ask about")
    }

    // --- the sweep refusing to guess -------------------------------------

    /**
     * The important one. An unreachable provider has told us nothing, and
     * resolving on a guess turns a temporary outage into wrongly-settled
     * balances — which, unlike the stuck row, running the job again later
     * does not fix.
     */
    @Test
    fun `an unreachable provider leaves the transaction alone`() {
        val h = harness(ScriptedProvider(throwOn = setOf("ref_1")))
        val id = stuckTransaction(h, Duration.ofHours(1))
        val tx = h.transactions.findById(project, id)!!
        val before = h.wallets.findById(project, tx.toWallet!!)!!.balanceCents

        val outcome = h.sweep.runOnce()

        assertEquals(1, outcome.unresolved)
        assertEquals(0, outcome.settled + outcome.failed)
        assertEquals(
            TxStatus.PENDING,
            h.transactions.findById(project, id)!!.status,
            "an exception is not evidence about the charge",
        )
        assertEquals(before, h.wallets.findById(project, tx.toWallet!!)!!.balanceCents)
    }

    @Test
    fun `an answer the adapter does not recognise resolves nothing`() {
        // ScriptedProvider returns UNKNOWN for anything unscripted.
        val h = harness(ScriptedProvider())
        val id = stuckTransaction(h, Duration.ofHours(1))

        assertEquals(1, h.sweep.runOnce().unresolved)
        assertEquals(TxStatus.PENDING, h.transactions.findById(project, id)!!.status)
    }

    /** Still authorized means still in flight — a normal hold, not unresolved. */
    @Test
    fun `a young authorized hold is left alone and is not unresolved`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.AUTHORIZED)))
        val id = stuckTransaction(h, Duration.ofMinutes(30))

        val outcome = h.sweep.runOnce()
        assertEquals(0, outcome.unresolved, "a normal hold must not page anyone")
        assertEquals(0, outcome.cancelled)
        assertEquals(TxStatus.PENDING, h.transactions.findById(project, id)!!.status)
    }

    /** An authorized hold past the expiry is voided: the ride is dead. */
    @Test
    fun `an authorized hold older than the expiry is cancelled`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.AUTHORIZED)))
        val id = stuckTransaction(h, Duration.ofHours(8))

        val outcome = h.sweep.runOnce()
        assertEquals(1, outcome.cancelled)
        assertEquals(TxStatus.CANCELLED, h.transactions.findById(project, id)!!.status)
    }

    /** D1: awaiting the customer's confirmation is a normal hold, too. */
    @Test
    fun `a young awaiting-customer hold is left alone`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.AWAITING_CUSTOMER)))
        val id = stuckTransaction(h, Duration.ofMinutes(30))

        val outcome = h.sweep.runOnce()
        assertEquals(0, outcome.unresolved)
        assertEquals(0, outcome.cancelled)
        assertEquals(TxStatus.PENDING, h.transactions.findById(project, id)!!.status)
    }

    // --- what it does not touch ------------------------------------------

    /**
     * The normal path settles in milliseconds, but the ride lifecycle that
     * drives settlement can legitimately take minutes. Sweeping a healthy
     * in-flight transaction would resolve a payment that was about to
     * resolve itself.
     */
    @Test
    fun `a recently created transaction is not swept`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.CAPTURED)))
        val id = stuckTransaction(h, Duration.ofMinutes(2))

        assertEquals(0, h.sweep.runOnce().total)
        assertEquals(TxStatus.PENDING, h.transactions.findById(project, id)!!.status)
        assertEquals(0, h.provider.lookups)
    }

    @Test
    fun `an empty sweep asks the provider nothing`() {
        val h = harness(ScriptedProvider())
        assertEquals(0, h.sweep.runOnce().total)
        assertEquals(0, h.provider.lookups)
    }

    /**
     * The batch limit bounds a pass. A sweep that tried to resolve a
     * backlog of thousands in one go would hold the provider's rate limit
     * for minutes and time out, achieving nothing.
     */
    @Test
    fun `the batch limit bounds one pass`() {
        val h = harness(ScriptedProvider(mapOf("ref_1" to ProviderStatus.CAPTURED)))
        repeat(5) { stuckTransaction(h, Duration.ofHours(1)) }

        assertEquals(2, h.sweep.runOnce(limit = 2).total)
        assertTrue(h.provider.lookups <= 2)
    }
}
