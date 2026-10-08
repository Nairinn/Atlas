package io.atlas.payments.grpc

import atlas.payments.CreateConnectAccountRequest
import atlas.payments.CreateConnectAccountResponse
import atlas.payments.DepositRequest
import atlas.payments.DepositResponse
import atlas.payments.DrainOutboxRequest
import atlas.payments.DrainOutboxResponse
import atlas.payments.GetConnectAccountRequest
import atlas.payments.GetConnectAccountResponse
import atlas.payments.PaymentsServiceGrpcKt
import atlas.payments.RefundRequest
import atlas.payments.RefundResponse
import atlas.payments.SettleRequest
import atlas.payments.SettleResponse
import atlas.payments.TransactionRequest
import atlas.payments.TransactionResponse
import atlas.payments.WalletRequest
import atlas.payments.WalletResponse
import io.atlas.payments.core.PaymentError
import io.atlas.payments.core.PaymentsService
import io.atlas.payments.outbox.OutboxDispatcher
import io.grpc.Status
import io.grpc.StatusException
import org.slf4j.LoggerFactory

/**
 * gRPC fronting [PaymentsService]. Translates the proto contract in
 * proto/payments.proto into domain calls and maps [PaymentError] subtypes to
 * gRPC status codes. DrainOutbox is wired to the [OutboxDispatcher] for
 * deterministic flushes (tests/ops); it is not exposed through the gateway.
 */
class PaymentsGrpcService(
    private val payments: PaymentsService,
    private val dispatcher: OutboxDispatcher,
) : PaymentsServiceGrpcKt.PaymentsServiceCoroutineImplBase() {

    override suspend fun deposit(request: DepositRequest): DepositResponse {
        val result = try {
            payments.deposit(
                projectId = request.projectId.toProjectId(),
                userId = request.userId,
                amountCents = request.amountCents,
                idempotencyKey = request.idempotencyKey,
            )
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return DepositResponse.newBuilder()
            .setTransactionId(result.transactionId.toString())
            .setStatus(result.status)
            .setBalanceCents(result.balanceCents)
            .build()
    }

    override suspend fun initiateTransaction(request: TransactionRequest): TransactionResponse {
        val result = try {
            payments.initiate(
                projectId = request.projectId.toProjectId(),
                fromUserId = request.fromUserId,
                toUserId = request.toUserId,
                amountCents = request.amountCents,
                idempotencyKey = request.idempotencyKey,
                rideId = request.rideId,
                applicationFeeCents = request.applicationFeeCents,
            )
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return TransactionResponse.newBuilder()
            .setTransactionId(result.transactionId.toString())
            .setStatus(result.status)
            // The app's client needs this to confirm the payment with
            // Stripe's client SDK (D1); the server only captures.
            .setClientSecret(result.clientSecret ?: "")
            .build()
    }

    override suspend fun settleTransaction(request: SettleRequest): SettleResponse {
        val result = try {
            payments.settle(request.projectId.toProjectId(), request.transactionId)
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return SettleResponse.newBuilder()
            .setSuccess(result.success)
            .setStatus(result.status)
            .build()
    }

    override suspend fun getWalletBalance(request: WalletRequest): WalletResponse {
        val balance = try {
            payments.walletBalance(request.projectId.toProjectId(), request.userId)
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return WalletResponse.newBuilder()
            .setBalanceCents(balance.balanceCents)
            .setCurrency(balance.currency)
            .build()
    }

    override suspend fun refundTransaction(request: RefundRequest): RefundResponse {
        val result = try {
            payments.refund(request.projectId.toProjectId(), request.transactionId)
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return RefundResponse.newBuilder()
            .setSuccess(result.success)
            .build()
    }

    override suspend fun drainOutbox(request: DrainOutboxRequest): DrainOutboxResponse {
        val dispatched = dispatcher.drainOnce(request.maxRows.toInt())
        return DrainOutboxResponse.newBuilder()
            .setRowsDispatched(dispatched)
            .build()
    }

    override suspend fun createConnectAccount(request: CreateConnectAccountRequest): CreateConnectAccountResponse {
        val url = try {
            payments.startConnectOnboarding(
                request.projectId.toProjectId(),
                request.userId,
                request.returnUrl,
            )
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return CreateConnectAccountResponse.newBuilder()
            .setOnboardingUrl(url)
            .build()
    }

    override suspend fun getConnectAccount(request: GetConnectAccountRequest): GetConnectAccountResponse {
        val status = try {
            payments.connectAccountStatus(request.projectId.toProjectId(), request.userId)
        } catch (e: PaymentError) {
            throw e.toGrpcStatusException()
        }
        return GetConnectAccountResponse.newBuilder()
            .setExists(status.exists)
            .setPayoutsEnabled(status.payoutsEnabled)
            .build()
    }

    companion object {
        private val LOG = LoggerFactory.getLogger(PaymentsGrpcService::class.java)
    }
}

private fun PaymentError.toGrpcStatusException(): StatusException = when (this) {
    // The caller named a user that is not theirs. FAILED_PRECONDITION, not
    // INVALID_ARGUMENT: the id is well-formed, it just does not describe
    // anyone they can pay.
    is PaymentError.UnknownUser ->
        Status.FAILED_PRECONDITION.withDescription(message).asException()

    is PaymentError.DriverNotOnboarded ->
        Status.FAILED_PRECONDITION.withDescription(message).asException()

    is PaymentError.InvalidAmount -> Status.INVALID_ARGUMENT.withDescription(message).asException()
    is PaymentError.InvalidArgument -> Status.INVALID_ARGUMENT.withDescription(message).asException()
    is PaymentError.TransactionNotFound -> Status.NOT_FOUND.withDescription(message).asException()
    is PaymentError.IdempotencyConflict -> Status.ALREADY_EXISTS.withDescription(message).asException()
    is PaymentError.InsufficientFunds -> Status.FAILED_PRECONDITION.withDescription(message).asException()
    is PaymentError.InvalidState -> Status.FAILED_PRECONDITION.withDescription(message).asException()
    is PaymentError.ProviderDeclined -> Status.FAILED_PRECONDITION.withDescription(message).asException()
}

/**
 * Parse the project the caller injected — the gateway for the RPCs it
 * routes, the fare-consumer (from the Kafka event) for settle and refund.
 *
 * Strict: an empty value cannot have come from a client, since no REST DTO
 * has the field. It can only mean a bug on the trusted side, and this is
 * the money path — defaulting would turn that bug into a charge against
 * the wrong tenant.
 */
private fun String.toProjectId(): java.util.UUID {
    if (isEmpty()) {
        throw io.grpc.Status.INVALID_ARGUMENT
            .withDescription("project_id is required; the caller must supply it")
            .asException()
    }
    return try {
        java.util.UUID.fromString(this)
    } catch (e: IllegalArgumentException) {
        throw io.grpc.Status.INVALID_ARGUMENT
            .withDescription("project_id is not a UUID")
            .asException()
    }
}
