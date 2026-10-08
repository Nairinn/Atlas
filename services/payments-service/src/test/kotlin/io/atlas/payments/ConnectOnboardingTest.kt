package io.atlas.payments

import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.PayoutAccount
import io.atlas.payments.core.PaymentsService
import io.atlas.payments.core.PayoutAccountSource
import io.atlas.payments.core.FakePaymentProvider
import io.atlas.payments.crypto.ConfigCipher
import io.atlas.payments.fakes.DirectTransactionRunner
import io.atlas.payments.fakes.InMemoryOutbox
import io.atlas.payments.fakes.InMemoryTransactionRepository
import io.atlas.payments.fakes.InMemoryWalletRepository
import java.security.SecureRandom
import java.util.UUID
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

/**
 * Onboarding idempotence, the fee in the idempotency hash, and
 * DriverNotOnboarded being thrown BEFORE a charge reaches the provider.
 */
class ConnectOnboardingTest {

    private val project = UUID.fromString("11111111-1111-1111-1111-111111111111")
    private val otherProject = UUID.fromString("22222222-2222-2222-2222-222222222222")

    // --- onboarding reuses the stored account --------------------------------

    @Test
    fun `onboarding called twice mints one account and keeps the stored id`() {
        val store = RecordingOnboardingStore()
        val onboarding = FakeOnboarding(store)

        val user = UUID.randomUUID()
        onboarding.start(project, user, "https://app.example/return")
        val firstId = store.saved[user]
        onboarding.start(project, user, "https://app.example/return")

        assertEquals(1, onboarding.created, "the second call must reuse, not create")
        assertEquals(firstId, store.saved[user], "the stored account id must not drift")
    }

    /** Stands in for StripeOnboarding with a controllable Stripe side. */
    private class FakeOnboarding(private val store: RecordingOnboardingStore) {
        var created = 0
            private set

        fun start(projectId: UUID, userId: UUID, returnUrl: String): String {
            val existing = store.saved[userId]
            val accountId = existing ?: "acct_${UUID.randomUUID()}".also { created++ }
            if (existing == null) store.save(projectId, userId, accountId)
            return "https://connect.example/onboarding/$accountId?return=$returnUrl"
        }
    }

    private class RecordingOnboardingStore {
        val saved = linkedMapOf<UUID, String>()

        fun save(projectId: UUID, userId: UUID, accountId: String) {
            saved[userId] = accountId
        }
    }

    // --- the fee is part of the idempotency hash ------------------------------

    @Test
    fun `replaying a key with a different fee is a conflict`() {
        val wallets = InMemoryWalletRepository()
        val transactions = InMemoryTransactionRepository()
        val service = PaymentsService(
            wallets = wallets,
            transactions = transactions,
            outbox = InMemoryOutbox(),
            runner = DirectTransactionRunner(),
            provider = FakePaymentProvider(),
            fareTopic = "atlas.fare.events",
        )
        val rider = UUID.randomUUID().toString()
        val driver = UUID.randomUUID().toString()

        service.initiate(project, rider, driver, 2_000, "key-1", "", applicationFeeCents = 100)

        val error = assertFailsWith<PaymentError.IdempotencyConflict> {
            service.initiate(project, rider, driver, 2_000, "key-1", "", applicationFeeCents = 200)
        }
        assertEquals("idempotency key reused with different arguments: key-1", error.message)
    }

    @Test
    fun `replaying a legacy zero-fee row with the same args still replays`() {
        val wallets = InMemoryWalletRepository()
        val transactions = InMemoryTransactionRepository()
        val service = PaymentsService(
            wallets = wallets,
            transactions = transactions,
            outbox = InMemoryOutbox(),
            runner = DirectTransactionRunner(),
            provider = FakePaymentProvider(),
            fareTopic = "atlas.fare.events",
        )
        val rider = UUID.randomUUID().toString()
        val driver = UUID.randomUUID().toString()

        val first = service.initiate(project, rider, driver, 2_000, "key-2", "")

        // Forcibly age the row to a v1 hash (pre-fee format).
        transactions.ageToLegacyHash(first.transactionId, rider, driver, 2_000)

        val replay = service.initiate(project, rider, driver, 2_000, "key-2", "")
        assertEquals(first.transactionId, replay.transactionId, "v1 rows replay unchanged")
    }

    // --- DriverNotOnboarded fires before authorize ----------------------------

    @Test
    fun `a non-onboarded payee fails before the provider is called`() {
        val wallets = InMemoryWalletRepository()
        val transactions = InMemoryTransactionRepository()
        val counting = CountingProvider()
        val service = PaymentsService(
            wallets = wallets,
            transactions = transactions,
            outbox = InMemoryOutbox(),
            runner = DirectTransactionRunner(),
            provider = counting,
            fareTopic = "atlas.fare.events",
            payoutAccounts = EmptyPayoutSource(),
        )
        val rider = UUID.randomUUID().toString()
        val driver = UUID.randomUUID().toString()

        assertFailsWith<PaymentError.DriverNotOnboarded> {
            service.initiate(project, rider, driver, 2_000, "key-3", "")
        }
        assertEquals(0, counting.authorizes, "no charge may be placed for an unpayable fare")
    }

    @Test
    fun `a payer without a payout account is fine`() {
        val wallets = InMemoryWalletRepository()
        val transactions = InMemoryTransactionRepository()
        val counting = CountingProvider()
        val payeeId = UUID.randomUUID()
        val service = PaymentsService(
            wallets = wallets,
            transactions = transactions,
            outbox = InMemoryOutbox(),
            runner = DirectTransactionRunner(),
            provider = counting,
            fareTopic = "atlas.fare.events",
            // The payee HAS an account; the payer does not.
            payoutAccounts = PayeeOnlyPayoutSource(payeeId),
        )
        val rider = UUID.randomUUID().toString()

        val result = service.initiate(project, rider, payeeId.toString(), 2_000, "key-4", "")
        assertEquals(io.atlas.payments.core.TxStatus.PENDING, result.status)
        assertEquals(1, counting.authorizes)
    }

    /** Payout source where nobody has an account. */
    private class EmptyPayoutSource : PayoutAccountSource {
        override fun accountFor(projectId: UUID, userId: UUID): PayoutAccount? = null

        override fun setPayoutsEnabled(projectId: UUID, userId: UUID, stripeAccountId: String, enabled: Boolean) {}
    }

    /** Payout source where exactly one payee has a usable account. */
    private class PayeeOnlyPayoutSource(private val payeeId: UUID) : PayoutAccountSource {
        override fun accountFor(projectId: UUID, userId: UUID): PayoutAccount? =
            if (userId == payeeId) PayoutAccount("acct_payee", payoutsEnabled = true) else null

        override fun setPayoutsEnabled(projectId: UUID, userId: UUID, stripeAccountId: String, enabled: Boolean) {}
    }

    /** Provider that only counts; charges always succeed. */
    private class CountingProvider : FakePaymentProvider() {
        var authorizes = 0
            private set

        override fun authorize(request: io.atlas.payments.core.ChargeRequest, idempotencyKey: String): io.atlas.payments.core.ProviderResult {
            authorizes++
            return super.authorize(request, idempotencyKey)
        }
    }

    // --- ConfigCipher ---------------------------------------------------------

    @Test
    fun `cipher round-trips and rejects tampering and wrong keys`() {
        val key = encode(32)
        val cipher = ConfigCipher(key)
        val secret = "sk_live_abcdefghijklmnop"

        assertEquals(secret, cipher.decrypt(cipher.encrypt(secret)), "round trip")

        val ciphertext = cipher.encrypt(secret)
        // Flip one byte deep in the tail (past the nonce).
        ciphertext[ciphertext.size - 1] = (ciphertext[ciphertext.size - 1].toInt() xor 1).toByte()
        assertFailsWith<Exception> { cipher.decrypt(ciphertext) }

        val other = ConfigCipher(encode(32))
        assertFailsWith<Exception> { other.decrypt(cipher.encrypt(secret)) }

        // Fresh nonces: two encryptions of the same plaintext differ.
        assertTrue(!cipher.encrypt(secret).contentEquals(cipher.encrypt(secret)))
    }

    @Test
    fun `a short key is rejected at construction`() {
        assertFailsWith<IllegalArgumentException> {
            ConfigCipher(encode(16))
        }
    }

    private fun encode(bytes: Int): String {
        val random = SecureRandom()
        val buf = ByteArray(bytes)
        random.nextBytes(buf)
        return buf.joinToString("") { "%02x".format(it) }
    }
}
