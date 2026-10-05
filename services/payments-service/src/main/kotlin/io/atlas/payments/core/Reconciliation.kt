package io.atlas.payments.core

import org.slf4j.LoggerFactory
import java.time.Clock
import java.time.Duration
import java.time.Instant

/**
 * Resolves transactions stuck in PENDING against the provider's own record.
 *
 * # Why a pending row is not merely untidy
 *
 * A transaction goes PENDING the moment the provider authorizes, and
 * leaves PENDING when it settles or fails. If the process dies in between
 * — a deploy, an OOM, a provider timeout after the charge went through —
 * the row stays PENDING forever. Nothing retries it, because nothing knows
 * whether retrying would be a second charge.
 *
 * That is money in limbo: the customer may have been charged and has no
 * balance to show for it, or may not have been and is owed nothing. Both
 * look identical from inside Atlas. The only source of truth is the
 * provider, which is why [PaymentProvider.lookup] exists.
 *
 * # What it refuses to do
 *
 * When the provider says UNKNOWN — unreachable, or an answer this adapter
 * does not recognise — the sweep leaves the row alone and counts it. It
 * does NOT pick a direction. Resolving on a guess is how a reconciliation
 * job turns a temporary provider outage into a pile of wrongly-settled
 * balances, and unlike the stuck row itself, that is not recoverable by
 * running the job again later.
 */
class ReconciliationSweep(
    private val transactions: TransactionRepository,
    private val wallets: WalletRepository,
    private val provider: PaymentProvider,
    private val runner: TransactionRunner,
    private val metrics: PaymentsMetrics = PaymentsMetrics.NOOP,
    /**
     * How long a transaction may sit PENDING before it is considered
     * stuck.
     *
     * Long enough that a slow-but-healthy capture is never swept: the
     * normal path settles in milliseconds, and the ride lifecycle that
     * drives settlement can legitimately take minutes. Fifteen minutes is
     * comfortably past both and still well inside the window where a
     * customer would notice.
     */
    private val stuckAfter: Duration = Duration.ofMinutes(15),
    private val clock: Clock = Clock.systemUTC(),
) {
    data class Outcome(
        val settled: Int = 0,
        val failed: Int = 0,
        val unresolved: Int = 0,
    ) {
        val total: Int get() = settled + failed + unresolved
    }

    /**
     * Run one pass. Returns what it did, so the caller can log or alert.
     *
     * [limit] bounds the batch: a sweep that tried to resolve a backlog of
     * ten thousand rows in one pass would hold the provider's rate limit
     * for minutes and time out, achieving nothing. Small batches converge.
     */
    fun runOnce(limit: Int = 100): Outcome {
        val cutoff = clock.instant().minus(stuckAfter)
        val stuck = transactions.findStuckPending(cutoff, limit)
        if (stuck.isEmpty()) return Outcome()

        LOG.info("reconciling {} transaction(s) pending since before {}", stuck.size, cutoff)

        var settled = 0
        var failed = 0
        var unresolved = 0

        for (tx in stuck) {
            // A pending transaction with no provider reference never
            // reached the provider at all, so there is nothing to ask
            // about and nothing was charged. Failing it is safe and is the
            // only outcome that frees the idempotency key.
            val ref = tx.providerRef
            if (ref.isNullOrBlank()) {
                transactions.markFailed(tx.projectId, tx.id, "no provider reference")
                failed++
                continue
            }

            val status = try {
                provider.lookup(tx.projectId, ref)
            } catch (e: Exception) {
                // An exception is not evidence of anything about the
                // charge, so it is treated exactly like UNKNOWN.
                LOG.warn("provider lookup failed for {}: {}", tx.id, e.message)
                ProviderStatus.UNKNOWN
            }

            when (status) {
                ProviderStatus.CAPTURED -> {
                    applySettlement(tx)
                    settled++
                }
                ProviderStatus.FAILED, ProviderStatus.NOT_FOUND -> {
                    transactions.markFailed(tx.projectId, tx.id, "provider reports $status")
                    failed++
                }
                // Still in flight: not stuck after all, just slow. Left
                // for the next pass rather than counted as a problem.
                ProviderStatus.AUTHORIZED -> unresolved++
                ProviderStatus.UNKNOWN -> {
                    LOG.warn(
                        "cannot resolve transaction {} (ref {}); leaving pending for a human",
                        tx.id, ref,
                    )
                    unresolved++
                }
            }
        }

        metrics.reconciled(settled = settled, failed = failed, unresolved = unresolved)
        return Outcome(settled, failed, unresolved)
    }

    /**
     * Apply the balance movement the original settle would have applied.
     *
     * Inside one transaction with the status change, for the same reason
     * the normal path is: a crash between moving the money and recording
     * that it moved leaves exactly the inconsistency this whole class
     * exists to clean up.
     */
    private fun applySettlement(tx: TxRecord) {
        runner.run {
            tx.fromWallet?.let { wallets.adjustBalance(tx.projectId, it, -tx.amountCents) }
            tx.toWallet?.let { wallets.adjustBalance(tx.projectId, it, tx.amountCents) }
            transactions.markSettled(tx.projectId, tx.id, clock.instant())
        }
        LOG.info("reconciled transaction {} as settled", tx.id)
    }

    private companion object {
        private val LOG = LoggerFactory.getLogger(ReconciliationSweep::class.java)
    }
}
