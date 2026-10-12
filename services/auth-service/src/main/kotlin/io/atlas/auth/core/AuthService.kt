package io.atlas.auth.core

import io.atlas.auth.crypto.JwtSigner
import io.atlas.auth.crypto.PasswordHasher
import java.time.Clock
import java.time.Duration
import java.util.UUID

/**
 * Transport-agnostic auth business logic. The gRPC layer wraps this in a gRPC
 * service. Tests inject in-memory repositories so the whole flow runs
 * without a database.
 *
 * Threading: instances are safe to share. The injected dependencies must
 * also be thread-safe (the production [PasswordHasher] and [JwtSigner] are;
 * the production Exposed-backed repositories rely on Exposed's transaction
 * semantics).
 */
class AuthService(
    private val users: UserRepository,
    private val sessions: SessionRepository,
    private val hasher: PasswordHasher,
    private val signer: JwtSigner,
    private val tokenLifetime: Duration = Duration.ofHours(1),
    private val clock: Clock = Clock.systemUTC(),
    private val verificationTokens: VerificationTokenRepository? = null,
    private val email: EmailSender? = null,
    /**
     * How long a reset link stays valid. Short, because the window is the
     * period during which someone with access to the mailbox — a shared
     * computer, a synced device, a forwarding rule — can take the account.
     */
    private val resetTokenLifetime: Duration = Duration.ofHours(1),
    /**
     * Longer, because the cost of an expired verification link is a minor
     * annoyance rather than a lost account, and people check mail late.
     */
    private val verificationTokenLifetime: Duration = Duration.ofHours(24),
) {
    /**
     * Creates a new user. Throws [AuthError.EmailAlreadyExists] if the email
     * is already taken, [AuthError.InvalidEmail] or [AuthError.WeakPassword]
     * for malformed input.
     */
    fun register(projectId: UUID, email: String, password: String): UUID {
        val normalized = normalizeEmail(email)
        validateEmail(normalized)
        validatePassword(password)
        if (users.findByEmail(projectId, normalized) != null) {
            throw AuthError.EmailAlreadyExists(normalized)
        }
        val hash = hasher.hash(password)
        return users.create(projectId = projectId, email = normalized, passwordHash = hash).id
    }

    /**
     * Verifies credentials and issues a signed token bound to a fresh
     * session. Throws [AuthError.InvalidCredentials] on any failure so the
     * API does not leak whether the email is registered.
     */
    fun authenticate(
        projectId: UUID,
        email: String,
        password: String,
        lastLat: Double? = null,
        lastLng: Double? = null,
    ): SignedToken {
        val normalized = normalizeEmail(email)
        // Scoped lookup, so presenting another project's credentials fails
        // as "no such user" rather than authenticating into the wrong
        // tenant. It also keeps the timing story the same as before: one
        // indexed lookup either way.
        val user = users.findByEmail(projectId, normalized) ?: throw AuthError.InvalidCredentials()
        if (!hasher.verify(password, user.passwordHash)) {
            throw AuthError.InvalidCredentials()
        }
        val now = clock.instant()
        val expiresAt = now.plus(tokenLifetime)
        val session = sessions.create(userId = user.id, issuedAt = now, expiresAt = expiresAt)
        val claims = TokenClaims(
            userId = user.id,
            // From the USER row, not from the caller's argument. They are
            // equal here because the lookup was scoped, but taking it from
            // the row means a token can only ever claim the project the
            // user actually belongs to.
            projectId = user.projectId,
            sessionId = session.id,
            issuedAt = now,
            expiresAt = expiresAt,
            lastLat = lastLat,
            lastLng = lastLng,
        )
        return SignedToken(token = signer.sign(claims), expiresAt = expiresAt)
    }

    /**
     * Refresh / re-issue primitive used by the internal `IssueToken` gRPC
     * call. The caller has already proven identity through some other
     * channel (typically a valid existing JWT) and is asking for a fresh
     * token tied to a new session.
     */
    fun issueTokenForUser(
        projectId: UUID,
        userId: UUID,
        lastLat: Double? = null,
        lastLng: Double? = null,
    ): SignedToken {
        val user = users.findById(projectId, userId) ?: throw AuthError.InvalidCredentials()
        val now = clock.instant()
        val expiresAt = now.plus(tokenLifetime)
        val session = sessions.create(userId = user.id, issuedAt = now, expiresAt = expiresAt)
        val claims = TokenClaims(
            userId = user.id,
            projectId = user.projectId,
            sessionId = session.id,
            issuedAt = now,
            expiresAt = expiresAt,
            lastLat = lastLat,
            lastLng = lastLng,
        )
        return SignedToken(token = signer.sign(claims), expiresAt = expiresAt)
    }

    // --- helpers ----------------------------------------------------------

    private fun normalizeEmail(email: String): String = email.trim().lowercase()

    private fun validateEmail(email: String) {
        // Intentionally permissive. RFC 5322 is unfeasibly large to validate
        // exactly; we just want to catch obvious typos and reject things
        // that clearly are not addresses.
        if (email.isEmpty()) throw AuthError.InvalidEmail(email)
        if (email.length > 254) throw AuthError.InvalidEmail(email)
        val at = email.indexOf('@')
        if (at <= 0 || at == email.length - 1) throw AuthError.InvalidEmail(email)
        val domain = email.substring(at + 1)
        if (!domain.contains('.')) throw AuthError.InvalidEmail(email)
        if (email.any { it.isWhitespace() }) throw AuthError.InvalidEmail(email)
    }

    private fun validatePassword(password: String) {
        if (password.length < MIN_PASSWORD_LENGTH) {
            throw AuthError.WeakPassword("must be at least $MIN_PASSWORD_LENGTH characters")
        }
        if (password.length > MAX_PASSWORD_LENGTH) {
            // bcrypt truncates at 72 bytes. Refusing longer inputs is more
            // honest than silently ignoring the tail.
            throw AuthError.WeakPassword("must be at most $MAX_PASSWORD_LENGTH characters")
        }
        if (password.isBlank()) {
            throw AuthError.WeakPassword("must contain non-whitespace characters")
        }
    }

    /**
     * Look up one user's current state.
     *
     * Scoped, so a user id from another project resolves to nothing rather
     * than to that project's user — the same rule every other lookup
     * follows.
     */
    fun getUser(projectId: UUID, userId: UUID): User =
        users.findById(projectId, userId) ?: throw AuthError.InvalidCredentials()

    // --- password reset ---------------------------------------------------

    /**
     * Mail a reset link, if the address belongs to a user in this project.
     *
     * # This method tells the caller nothing
     *
     * It returns Unit and takes the same path whether or not the address
     * exists. That is deliberate and it is the whole security property:
     * an endpoint that answers "no such user" is an account enumeration
     * oracle, and the addresses it confirms are exactly the ones worth
     * attacking. The user who genuinely mistyped their address learns
     * nothing either — which is the cost, and it is smaller than the
     * alternative.
     */
    fun requestPasswordReset(projectId: UUID, email: String) {
        val tokens = requireTokens()
        val sender = requireEmail()
        val normalized = normalizeEmail(email)
        val user = users.findByEmail(projectId, normalized)
            ?: return  // Silently. See above.

        val now = clock.instant()
        // Any earlier link stops working the moment a new one is issued,
        // so a user who clicks "resend" three times cannot leave three
        // live ways into their account lying in a mailbox.
        tokens.invalidateAll(projectId, user.id, TokenPurpose.PASSWORD_RESET, now)

        val plaintext = VerificationTokens.generate()
        tokens.create(
            projectId = projectId,
            userId = user.id,
            purpose = TokenPurpose.PASSWORD_RESET,
            tokenHash = VerificationTokens.hash(plaintext),
            expiresAt = now.plus(resetTokenLifetime),
        )

        sender.send(
            EmailMessage(
                to = user.email,
                subject = "Reset your password",
                body = "Use this token to set a new password. It expires in " +
                    "${resetTokenLifetime.toHours()} hour(s) and can be used once:" +
                    "\n\n$plaintext\n\n" +
                    "If you did not ask for this, you can ignore it — your " +
                    "password has not changed.",
            ),
        )
    }

    /**
     * Redeem a reset token and set a new password.
     *
     * Throws [AuthError.TokenInvalid] for anything wrong with the token —
     * unknown, expired, already used, or for a different purpose. One
     * error for all four cases on purpose: distinguishing them tells a
     * holder of a guessed token which guesses are getting warmer.
     */
    fun resetPassword(token: String, newPassword: String): UUID {
        val tokens = requireTokens()
        validatePassword(newPassword)

        val now = clock.instant()
        val row = tokens.findByHash(VerificationTokens.hash(token))
            ?: throw AuthError.TokenInvalid("invalid or expired token")
        if (row.purpose != TokenPurpose.PASSWORD_RESET) {
            // A verification token must not double as a reset token: they
            // are mailed under different pretexts and one is far easier to
            // get a user to click.
            throw AuthError.TokenInvalid("invalid or expired token")
        }
        if (row.usedAt != null || !now.isBefore(row.expiresAt)) {
            throw AuthError.TokenInvalid("invalid or expired token")
        }

        // Consume FIRST. If two clicks arrive together this is the
        // conditional UPDATE that lets exactly one through; doing the
        // password change first would let both apply, and the second
        // would silently overwrite the first user's chosen password.
        if (!tokens.consume(row.id, now)) {
            throw AuthError.TokenInvalid("invalid or expired token")
        }

        users.updatePasswordHash(row.projectId, row.userId, hasher.hash(newPassword))

        // Everything else that was outstanding stops working: other reset
        // links, and every live session. A reset is a statement that the
        // old credential may be compromised, so a session established with
        // it must not survive.
        tokens.invalidateAll(row.projectId, row.userId, TokenPurpose.PASSWORD_RESET, now)
        sessions.revokeAllForUser(row.userId)

        return row.userId
    }

    // --- email verification -----------------------------------------------

    /**
     * Mail a verification link. Silent for an unknown address and for one
     * already verified, for the same enumeration reason as above.
     */
    fun requestEmailVerification(projectId: UUID, email: String) {
        val tokens = requireTokens()
        val sender = requireEmail()
        val normalized = normalizeEmail(email)
        val user = users.findByEmail(projectId, normalized) ?: return
        if (user.emailVerifiedAt != null) return

        val now = clock.instant()
        tokens.invalidateAll(projectId, user.id, TokenPurpose.EMAIL_VERIFICATION, now)

        val plaintext = VerificationTokens.generate()
        tokens.create(
            projectId = projectId,
            userId = user.id,
            purpose = TokenPurpose.EMAIL_VERIFICATION,
            tokenHash = VerificationTokens.hash(plaintext),
            expiresAt = now.plus(verificationTokenLifetime),
        )

        sender.send(
            EmailMessage(
                to = user.email,
                subject = "Confirm your email address",
                body = "Use this token to confirm this address. It expires in " +
                    "${verificationTokenLifetime.toHours()} hours:\n\n$plaintext\n",
            ),
        )
    }

    /**
     * Redeem a verification token.
     *
     * Unlike a reset this does NOT revoke sessions: confirming an address
     * is not evidence the password leaked, and logging someone out for
     * doing what they were asked to do is a bad trade.
     */
    fun verifyEmail(token: String): UUID {
        val tokens = requireTokens()
        val now = clock.instant()

        val row = tokens.findByHash(VerificationTokens.hash(token))
            ?: throw AuthError.TokenInvalid("invalid or expired token")
        if (row.purpose != TokenPurpose.EMAIL_VERIFICATION) {
            throw AuthError.TokenInvalid("invalid or expired token")
        }
        if (row.usedAt != null || !now.isBefore(row.expiresAt)) {
            throw AuthError.TokenInvalid("invalid or expired token")
        }
        if (!tokens.consume(row.id, now)) {
            throw AuthError.TokenInvalid("invalid or expired token")
        }

        users.markEmailVerified(row.projectId, row.userId, now)
        return row.userId
    }

    // --- helpers ----------------------------------------------------------

    /**
     * These flows are optional at construction, so a deployment with no
     * mail provider still serves login and registration. Reaching one
     * without the dependency wired is a 412 naming the missing
     * configuration — far better than a reset endpoint that accepts the
     * request and silently does nothing, which is what a user experiences
     * as "the email never arrived".
     */
    private fun requireTokens(): VerificationTokenRepository =
        verificationTokens ?: throw AuthError.EmailNotConfigured()

    private fun requireEmail(): EmailSender =
        email ?: throw AuthError.EmailNotConfigured()

    companion object {
        const val MIN_PASSWORD_LENGTH = 8
        const val MAX_PASSWORD_LENGTH = 72
    }
}
