package io.atlas.auth.core

/**
 * Sealed hierarchy of auth-domain errors. The gRPC layer maps
 * each subtype to the appropriate status code; this layer is transport-
 * agnostic.
 *
 * [InvalidCredentials] deliberately does not distinguish "unknown email"
 * from "wrong password" so the API cannot be used to enumerate registered
 * emails.
 */
sealed class AuthError(message: String) : Exception(message) {
    /**
     * A flow was used that needs an email provider, and none is wired.
     *
     * This is deliberately a per-CALL error rather than a startup failure.
     * Login and registration do not need email; refusing to boot the whole
     * service because password reset is unconfigured would take down
     * authentication for everyone to protect a feature nobody was using.
     * The feature that needs the provider fails; nothing else does.
     */
    class EmailNotConfigured :
        AuthError(
            "email delivery is not configured; password reset and email " +
                "verification are unavailable",
        )

    class EmailAlreadyExists(email: String) : AuthError("email already registered: $email")
    class InvalidEmail(email: String) : AuthError("invalid email address: $email")
    class WeakPassword(reason: String) : AuthError("weak password: $reason")
    class InvalidCredentials : AuthError("invalid email or password")
    class TokenInvalid(reason: String) : AuthError("token invalid: $reason")
    class SessionRevoked : AuthError("session has been revoked")
    class SessionExpired : AuthError("session has expired")
}
