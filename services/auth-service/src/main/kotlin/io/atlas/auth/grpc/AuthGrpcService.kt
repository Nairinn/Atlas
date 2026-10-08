package io.atlas.auth.grpc

import atlas.auth.AuthRequest
import atlas.auth.AuthServiceGrpcKt
import atlas.auth.IssueTokenRequest
import atlas.auth.GetUserRequest
import atlas.auth.RegisterRequest
import atlas.auth.RegisterResponse
import atlas.auth.RequestEmailVerificationRequest
import atlas.auth.RequestEmailVerificationResponse
import atlas.auth.RequestPasswordResetRequest
import atlas.auth.RequestPasswordResetResponse
import atlas.auth.ResetPasswordRequest
import atlas.auth.ResetPasswordResponse
import atlas.auth.VerifyEmailRequest
import atlas.auth.VerifyEmailResponse
import atlas.auth.RevokeResponse
import atlas.auth.RevokeTokenRequest
import atlas.auth.TokenResponse
import atlas.auth.UserProfile
import atlas.auth.ValidateTokenRequest
import io.atlas.auth.cache.TokenValidationCache
import io.atlas.auth.core.AuthError
import io.atlas.auth.core.AuthMetrics
import io.atlas.auth.core.AuthService
import io.atlas.auth.core.SessionRepository
import io.atlas.auth.core.SignedToken
import io.atlas.auth.crypto.JwtSigner
import io.atlas.auth.kafka.AuthTokenEventPublisher
import io.atlas.auth.kafka.sha256Hex
import io.grpc.Status
import io.grpc.StatusException
import org.slf4j.LoggerFactory
import java.time.Clock
import java.util.UUID
import io.atlas.auth.core.TokenClaims as DomainTokenClaims
import atlas.auth.TokenClaims as ProtoTokenClaims

/**
 * gRPC fronting [AuthService]. Translates the domain API into the proto
 * contract defined in proto/auth.proto, maps domain errors to gRPC status
 * codes, and threads two side effects through every call:
 *
 *  1. A 30s in-process [TokenValidationCache] so ValidateToken does not hit
 *     Postgres on every gateway request. The cache is keyed by JWT signature
 *     segment.
 *  2. Kafka [AuthTokenEventPublisher] fanout on Authenticate, IssueToken,
 *     and RevokeToken so other instances can evict their caches and so
 *     downstream consumers see token lifecycle. Register does not produce a
 *     token, so it does not publish an AuthTokenEvent.
 */
class AuthGrpcService(
    private val authService: AuthService,
    private val sessions: SessionRepository,
    private val signer: JwtSigner,
    private val cache: TokenValidationCache,
    private val publisher: AuthTokenEventPublisher,
    private val metrics: AuthMetrics = AuthMetrics.NOOP,
    private val clock: Clock = Clock.systemUTC(),
) : AuthServiceGrpcKt.AuthServiceCoroutineImplBase() {

    override suspend fun register(request: RegisterRequest): RegisterResponse {
        val projectId = request.projectId.toProjectId()
        val userId = try {
            authService.register(projectId, request.email, request.password)
        } catch (e: AuthError) {
            throw e.toGrpcStatusException()
        }
        metrics.userRegistered()
        return RegisterResponse.newBuilder().setUserId(userId.toString()).build()
    }

    override suspend fun authenticate(request: AuthRequest): TokenResponse {
        val projectId = request.projectId.toProjectId()
        val signed = try {
            authService.authenticate(
                projectId = projectId,
                email = request.email,
                password = request.password,
                lastLat = if (request.lat == 0.0 && request.lng == 0.0) null else request.lat,
                lastLng = if (request.lat == 0.0 && request.lng == 0.0) null else request.lng,
            )
        } catch (e: AuthError) {
            metrics.authenticationFailed(e.metricReason())
            throw e.toGrpcStatusException()
        }
        metrics.authenticated()
        metrics.tokenIssued()
        publishIssued(signed)
        return signed.toResponse()
    }

    override suspend fun issueToken(request: IssueTokenRequest): TokenResponse {
        val userId = try {
            UUID.fromString(request.userId)
        } catch (e: IllegalArgumentException) {
            throw Status.INVALID_ARGUMENT.withDescription("user_id is not a UUID").asException()
        }
        val signed = try {
            authService.issueTokenForUser(
                projectId = request.projectId.toProjectId(),
                userId = userId,
                lastLat = if (request.lat == 0.0 && request.lng == 0.0) null else request.lat,
                lastLng = if (request.lat == 0.0 && request.lng == 0.0) null else request.lng,
            )
        } catch (e: AuthError) {
            throw e.toGrpcStatusException()
        }
        metrics.tokenIssued()
        publishIssued(signed)
        return signed.toResponse()
    }

    override suspend fun validateToken(request: ValidateTokenRequest): ProtoTokenClaims {
        val token = request.token
        cache.get(token)?.let {
            // Served without touching Postgres. The hit/miss ratio is the
            // early-warning signal for database load on this path.
            metrics.tokenValidated(cacheHit = true)
            return it.toProto()
        }

        val domainClaims = try {
            signer.verify(token)
        } catch (e: AuthError) {
            metrics.tokenRejected(e.metricReason())
            throw e.toGrpcStatusException()
        }

        val session = sessions.findById(domainClaims.sessionId)
            ?: run {
                metrics.tokenRejected("session_not_found")
                throw Status.UNAUTHENTICATED.withDescription("session not found").asException()
            }
        if (session.revoked) {
            metrics.tokenRejected("session_revoked")
            throw Status.PERMISSION_DENIED.withDescription("session revoked").asException()
        }
        if (clock.instant().isAfter(session.expiresAt)) {
            metrics.tokenRejected("session_expired")
            throw Status.UNAUTHENTICATED.withDescription("session expired").asException()
        }
        metrics.tokenValidated(cacheHit = false)

        cache.putWithHash(token, domainClaims, sha256Hex(token))
        return domainClaims.toProto()
    }

    override suspend fun revokeToken(request: RevokeTokenRequest): RevokeResponse {
        val token = request.token
        val domainClaims = try {
            signer.verify(token)
        } catch (e: AuthError) {
            // Revoking an already-invalid token is idempotent and not an
            // error from the caller's perspective; surface success=false.
            return RevokeResponse.newBuilder().setSuccess(false).build()
        }
        sessions.revoke(domainClaims.sessionId)
        cache.evict(token)
        metrics.tokenRevoked()
        try {
            publisher.publishRevoked(domainClaims, token)
        } catch (e: Exception) {
            LOG.warn("publishRevoked threw; revocation still persisted", e)
        }
        return RevokeResponse.newBuilder().setSuccess(true).build()
    }

    override suspend fun getUser(request: GetUserRequest): UserProfile {
        val projectId = request.projectId.toProjectId()
        val userId = try {
            UUID.fromString(request.userId)
        } catch (e: IllegalArgumentException) {
            throw Status.INVALID_ARGUMENT.withDescription("user_id is not a UUID").asException()
        }
        val user = try {
            authService.getUser(projectId, userId)
        } catch (e: AuthError) {
            throw e.toGrpcStatusException()
        }
        return UserProfile.newBuilder()
            .setUserId(user.id.toString())
            .setEmail(user.email)
            .setCreatedAt(user.createdAt.epochSecond)
            // 0 for unverified: proto3 has no optional scalars, and no
            // address was ever confirmed at the epoch.
            .setEmailVerifiedAt(user.emailVerifiedAt?.epochSecond ?: 0L)
            .build()
    }

    // --- password reset / email verification -------------------------------

    override suspend fun requestPasswordReset(
        request: RequestPasswordResetRequest,
    ): RequestPasswordResetResponse {
        val projectId = request.projectId.toProjectId()
        try {
            authService.requestPasswordReset(projectId, request.email)
        } catch (e: AuthError) {
            // Reaching here means a validation error on the address, which
            // is still an answer about whether it could exist. Swallow it:
            // the response is identical either way, by design.
            LOG.debug("password reset request rejected: {}", e.metricReason())
        }
        metrics.passwordResetRequested()
        return RequestPasswordResetResponse.getDefaultInstance()
    }

    override suspend fun resetPassword(request: ResetPasswordRequest): ResetPasswordResponse {
        val userId = try {
            authService.resetPassword(request.token, request.newPassword)
        } catch (e: AuthError) {
            metrics.passwordResetRejected(e.metricReason())
            throw e.toGrpcStatusException()
        }
        metrics.passwordReset()
        // Every session for this user was just revoked, so the validation
        // cache must forget them too — otherwise a token issued before the
        // reset keeps validating from memory for the rest of the TTL,
        // which is exactly the window the reset exists to close. This
        // instance evicts directly; the fanout tells peer replicas, whose
        // caches hold their own copies of the same tokens.
        cache.evictUser(userId)
        try {
            publisher.publishUserRevoked(userId)
        } catch (e: Exception) {
            LOG.warn("user-wide revocation fanout failed; peers recover via the 30s TTL", e)
        }
        return ResetPasswordResponse.newBuilder().setUserId(userId.toString()).build()
    }

    override suspend fun requestEmailVerification(
        request: RequestEmailVerificationRequest,
    ): RequestEmailVerificationResponse {
        val projectId = request.projectId.toProjectId()
        try {
            authService.requestEmailVerification(projectId, request.email)
        } catch (e: AuthError) {
            LOG.debug("verification request rejected: {}", e.metricReason())
        }
        return RequestEmailVerificationResponse.getDefaultInstance()
    }

    override suspend fun verifyEmail(request: VerifyEmailRequest): VerifyEmailResponse {
        val userId = try {
            authService.verifyEmail(request.token)
        } catch (e: AuthError) {
            throw e.toGrpcStatusException()
        }
        metrics.emailVerified()
        return VerifyEmailResponse.newBuilder().setUserId(userId.toString()).build()
    }

    // --- helpers ----------------------------------------------------------

    private fun publishIssued(signed: SignedToken) {
        val claims = try {
            signer.verify(signed.token)
        } catch (e: AuthError) {
            // We just signed it; this is unreachable barring a config bug.
            LOG.error("could not verify token we just issued", e)
            return
        }
        cache.putWithHash(signed.token, claims, sha256Hex(signed.token))
        try {
            publisher.publishIssued(claims, signed.token)
        } catch (e: Exception) {
            LOG.warn("publishIssued threw; token still valid", e)
        }
    }

    companion object {
        private val LOG = LoggerFactory.getLogger(AuthGrpcService::class.java)
    }
}

private fun SignedToken.toResponse(): TokenResponse =
    TokenResponse.newBuilder()
        .setToken(token)
        .setExpiresAt(expiresAt.epochSecond)
        .build()

/**
 * Parse the project the gateway injected.
 *
 * This is a trusted-side check, and it is deliberately strict. Everything
 * behind the gateway trusts its callers, so an empty or malformed
 * project_id cannot have come from a client — it can only mean a bug on
 * the trusted side. Defaulting to anything here would turn that bug into
 * silent cross-tenant writes; failing the call turns it into a stack
 * trace, which is what you want from a bug you have not found yet.
 */
private fun String.toProjectId(): UUID {
    if (isEmpty()) {
        throw Status.INVALID_ARGUMENT
            .withDescription("project_id is required; the gateway must inject it")
            .asException()
    }
    return try {
        UUID.fromString(this)
    } catch (e: IllegalArgumentException) {
        throw Status.INVALID_ARGUMENT.withDescription("project_id is not a UUID").asException()
    }
}

private fun DomainTokenClaims.toProto(): ProtoTokenClaims =
    ProtoTokenClaims.newBuilder()
        .setUserId(userId.toString())
        .setProjectId(projectId.toString())
        .setSessionId(sessionId.toString())
        .setIssuedAt(issuedAt.epochSecond)
        .setExpiresAt(expiresAt.epochSecond)
        .setLastLat(lastLat ?: 0.0)
        .setLastLng(lastLng ?: 0.0)
        .build()

/**
 * A low-cardinality label for metrics. Deliberately the error KIND and never
 * `message`, which interpolates the email address or a token reason and would
 * mint an unbounded number of Prometheus time series — as well as putting user
 * data into a metrics endpoint.
 */
private fun AuthError.metricReason(): String = when (this) {
    is AuthError.EmailAlreadyExists -> "email_exists"
    is AuthError.InvalidEmail -> "invalid_email"
    is AuthError.WeakPassword -> "weak_password"
    is AuthError.InvalidCredentials -> "invalid_credentials"
    is AuthError.TokenInvalid -> "token_invalid"
    is AuthError.SessionRevoked -> "session_revoked"
    is AuthError.SessionExpired -> "session_expired"
    is AuthError.EmailNotConfigured -> "email_not_configured"
}

private fun AuthError.toGrpcStatusException(): StatusException = when (this) {
    is AuthError.EmailAlreadyExists -> Status.ALREADY_EXISTS.withDescription(message).asException()
    is AuthError.InvalidEmail -> Status.INVALID_ARGUMENT.withDescription(message).asException()
    is AuthError.WeakPassword -> Status.INVALID_ARGUMENT.withDescription(message).asException()
    is AuthError.InvalidCredentials -> Status.UNAUTHENTICATED.withDescription(message).asException()
    is AuthError.TokenInvalid -> Status.UNAUTHENTICATED.withDescription(message).asException()
    is AuthError.SessionRevoked -> Status.PERMISSION_DENIED.withDescription(message).asException()
    is AuthError.SessionExpired -> Status.UNAUTHENTICATED.withDescription(message).asException()
    // The caller did nothing wrong and retrying will not help until an
    // operator configures a provider, which is exactly FAILED_PRECONDITION.
    is AuthError.EmailNotConfigured ->
        Status.FAILED_PRECONDITION.withDescription(message).asException()
}
