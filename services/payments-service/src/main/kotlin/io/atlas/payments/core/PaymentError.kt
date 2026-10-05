package io.atlas.payments.core

import java.util.UUID

/**
 * Sealed hierarchy of payment-domain errors. The gRPC layer maps each subtype
 * to the appropriate status code; this layer is transport-agnostic.
 */
sealed class PaymentError(message: String) : Exception(message) {
    class InvalidAmount(amount: Long) : PaymentError("amount_cents must be positive: $amount")
    class InvalidArgument(reason: String) : PaymentError(reason)
    class TransactionNotFound(id: String) : PaymentError("transaction not found: $id")
    class IdempotencyConflict(key: String) :
        PaymentError("idempotency key reused with different arguments: $key")
    class InsufficientFunds(walletId: UUID) : PaymentError("insufficient funds in wallet $walletId")
    class InvalidState(reason: String) : PaymentError(reason)
    class ProviderDeclined(reason: String) : PaymentError("payment provider declined: $reason")

    /**
     * The named user is not a member of this project.
     *
     * Raised when the composite (project_id, user_id) foreign key added by
     * migration 0080 rejects a wallet. Deliberately the same answer for
     * "no such user" and "that user belongs to another customer":
     * distinguishing them would turn the transfer endpoint into an oracle
     * for which user ids exist elsewhere on the platform.
     */
    class UnknownUser(userId: UUID) : PaymentError("no such user in this project: $userId")

    /**
     * A charge named a payee who has not finished connecting a payout
     * account. FAILED_PRECONDITION rather than a provider decline: the
     * payer did nothing wrong and no money moved — the app should send
     * the payee to onboarding, not show a card error.
     */
    class DriverNotOnboarded(userId: UUID) :
        PaymentError("payee $userId has no connected payout account; send them to onboarding first")
}

/**
 * Thrown by [TransactionRepository.insertPending] when the unique idempotency
 * key already exists (a lost race). The service layer reloads the winning row.
 */
class DuplicateIdempotencyKey(key: String) : RuntimeException("idempotency key already exists: $key")
