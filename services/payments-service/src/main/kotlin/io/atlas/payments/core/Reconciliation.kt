package io.atlas.payments.core

import org.slf4j.LoggerFactory
import java.time.Clock
import java.time.Duration

/**
 * Resolves transactions stuck in PENDING against the provider's own record.
 *
 * A transaction goes PENDING when the provider authorizes and leaves
 * PENDING when it settles, fails, or is cancelled. If the process dies in
 * between, the row stays PENDING and nothing retries it, because nothing
 * knows whether retrying would be a second charge. The provider is the
 * only source of truth, which is why [PaymentProvider.lookup] exists.
 *
 * When the provider is UNKNOWN the sweep leaves the row alone and counts
 * it: resolving on a guess is how a temporary provider outage becomes a
 * pile of wrongly-settled balances, and that damage is not fixed by
 * running the job again later.
 *
 * AUTHORIZED and AWAITING_CUSTOMER rows are normal holds, not problems:
 * they are counted separately so `AtlasPaymentsUnreconciled` fires only
 * on money whose fate nobody knows. A hold older than [holdExpiry] is
 * cancelled — voided at the provider — because an authorization Stripe
 * keeps open past expiry is a bug magnet, and holds expire by themselves
 * anyway.
 */
class ReconciliationSweep(
    private val transactions: TransactionRepository,
    private val applier: SettlementApplier,
    private val provider: PaymentProvider,
    private val metrics: PaymentsMetrics = PaymentsMetrics.NOOP,
    /**
     * How long a transaction may sit PENDING before the sweep looks at it.
     * The ride lifecycle that drives settlement can legitimately take
     * minutes; fifteen minutes is past that and inside the window where a
     * customer would notice.
     */
    private val stuckAfter: Duration = Duration.ofMinutes(15),
    /**
     * How old an authorized-but-uncaptured hold may get before the sweep
     * cancels it. Should be shorter than the provider's own auth expiry.
     */
    private val holdExpiry: Duration = Duration.ofHours(6),
    private val clock: Clock = Clock.systemUTC(),
) {
    data class Outcome(
        val settled: Int = 0,
        val failed: Int = 0,
        val cancelled: Int = 0,
        /** Money whose fate nobody knows — the number worth alerting on. */
        val unresolved: Int = 0,
    ) {
        val total: Int get() = settled + failed + cancelled + unresolved
    }

    /**
     * Run one pass. [limit] bounds the batch: a sweep that tried to
     * resolve a backlog of ten thousand rows in one pass would hold the
     * provider's rate limit for minutes and time out. Small batches
     * converge.
     */
    fun runOnce(limit: Int = 100): Outcome {
        val cutoff = clock.instant().minus(stuckAfter)
        val stuck = transactions.findStuckPending(cutoff, limit)
        if (stuck.isEmpty()) return Outcome()

        LOG.info("reconciling {} transaction(s) pending since before {}", stuck.size, cutoff)

        var settled = 0
        var failed = 0
        var cancelled = 0
        var unresolved = 0

        for (tx in stuck) {
            // A pending transaction with no provider reference never
            // reached the provider, so nothing was charged. Failing it is
            // safe and frees the idempotency key.
            val ref = tx.providerRef
            if (ref.isNullOrBlank()) {
                if (transactions.transitionFailed(tx.projectId, tx.id, "no provider reference") > 0) failed++
                continue
            }

            val status = try {
                provider.lookup(tx.projectId, ref)
            } catch (e: Exception) {
                // An exception is not evidence about the charge; same as UNKNOWN.
                LOG.warn("provider lookup failed for {}: {}", tx.id, e.message)
                ProviderStatus.UNKNOWN
            }

            when (status) {
                ProviderStatus.CAPTURED -> {
                    if (applier.applySettlement(tx)) settled++
                }
                ProviderStatus.FAILED, ProviderStatus.NOT_FOUND -> {
                    if (transactions.transitionFailed(tx.projectId, tx.id, "provider reports $status") > 0) failed++
                }
                // Still in flight: not stuck after all, just slow.
                ProviderStatus.AUTHORIZED, ProviderStatus.AWAITING_CUSTOMER -> {
                    if (isOlderThan(tx, holdExpiry)) {
                        if (applier.applyCancellation(tx, "hold expired after $holdExpiry")) cancelled++
                    }
                    // Younger holds are normal and counted as nothing.
                }
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
        return Outcome(settled, failed, cancelled, unresolved)
    }

    private fun isOlderThan(tx: TxRecord, age: Duration): Boolean {
        val created = tx.createdAt ?: return false
        return created.isBefore(clock.instant().minus(age))
    }

    private companion object {
        private val LOG = LoggerFactory.getLogger(ReconciliationSweep::class.java)
    }
}
