package io.atlas.auth.crypto

import io.atlas.auth.core.AuthError
import io.atlas.auth.core.TokenClaims
import org.jose4j.jws.AlgorithmIdentifiers
import org.jose4j.jws.JsonWebSignature
import org.jose4j.jwt.JwtClaims
import org.jose4j.jwt.consumer.InvalidJwtException
import org.jose4j.jwt.consumer.JwtConsumerBuilder
import org.jose4j.keys.HmacKey
import java.time.Instant
import java.util.UUID

/**
 * Signs and verifies Atlas JWTs.
 *
 * Token shape:
 * ```
 * {
 *   "sub": "<user_id>",
 *   "iat": <unix>,
 *   "exp": <unix>,
 *   "atlas:project_id": "<project_id>",
 *   "atlas:session_id": "<session_id>",
 *   "atlas:last_lat": <double>,        // optional
 *   "atlas:last_lng": <double>         // optional
 * }
 * ```
 *
 * HS256 in development. In production this rotates to RS256 with GCP KMS as
 * noted in the spec; the [JwtSigner] interface stays the same so the rest of
 * the service does not change.
 */
interface JwtSigner {
    fun sign(claims: TokenClaims): String

    /**
     * Returns parsed claims if the token is well-formed, signed by us, and
     * unexpired. Throws [AuthError.TokenInvalid] otherwise. This method
     * deliberately does not consult the session store; revocation is a
     * separate check performed by callers.
     */
    fun verify(token: String): TokenClaims
}

/**
 * One signing key and its identifier.
 *
 * The id travels in the JWT's `kid` header so a verifier knows which key
 * to try, instead of trying all of them and treating "none worked" as
 * both "wrong key" and "forged token".
 */
/** Fixed values for iss/aud: Atlas is the only issuer and only audience
 * of its own tokens, and pinning both closes the "same key, different
 * purpose" confusion a shared-secret design otherwise allows. */
private const val ISSUER = "atlas"
private const val AUDIENCE = "atlas.auth"

data class SigningKey(val id: String, val secret: String) {
    init {
        require(id.isNotBlank()) { "signing key id must not be blank" }
        require(secret.toByteArray(Charsets.UTF_8).size >= 32) {
            "JWT secret must be at least 32 BYTES for HS256; got ${secret.toByteArray(Charsets.UTF_8).size}"
        }
    }
}

/**
 * HS256 signer supporting key rotation.
 *
 * # Why rotation needs two keys live at once
 *
 * A single-secret signer cannot be rotated. Changing the secret
 * invalidates every token signed with the old one at the instant of the
 * change, so every user is logged out simultaneously — during a
 * deployment, when replicas are already restarting, and typically in
 * response to a suspected leak, which is the worst moment to also take
 * down authentication.
 *
 * With [retired] keys the rotation is boring:
 *
 *   1. Deploy the new key as retired. Nothing signs with it; every replica
 *      can now verify it.
 *   2. Promote it to active. New tokens carry the new `kid`; tokens signed
 *      with the old key still verify, because the old key is now retired
 *      rather than gone.
 *   3. After the maximum token lifetime, drop the old key entirely.
 *
 * Each step is independently safe to roll back, which is what makes it
 * runnable at 3am.
 */
class Jose4jJwtSigner(
    private val active: SigningKey,
    private val retired: List<SigningKey> = emptyList(),
) : JwtSigner {

    /** Convenience for tests and single-key deployments. */
    constructor(secret: String) : this(SigningKey(DEFAULT_KEY_ID, secret))

    private val keysById: Map<String, HmacKey> =
        (listOf(active) + retired).associate { it.id to HmacKey(it.secret.toByteArray(Charsets.UTF_8)) }

    init {
        require((listOf(active) + retired).map { it.id }.toSet().size == 1 + retired.size) {
            "signing key ids must be unique; a repeated kid makes verification ambiguous"
        }
    }

    private val key = keysById.getValue(active.id)

    override fun sign(claims: TokenClaims): String {
        val jwt = JwtClaims().apply {
            issuer = ISSUER
            setAudience(listOf(AUDIENCE))
            subject = claims.userId.toString()
            issuedAt = org.jose4j.jwt.NumericDate.fromSeconds(claims.issuedAt.epochSecond)
            expirationTime = org.jose4j.jwt.NumericDate.fromSeconds(claims.expiresAt.epochSecond)
            setClaim(CLAIM_PROJECT_ID, claims.projectId.toString())
            setClaim(CLAIM_SESSION_ID, claims.sessionId.toString())
            if (claims.lastLat != null) setClaim(CLAIM_LAST_LAT, claims.lastLat)
            if (claims.lastLng != null) setClaim(CLAIM_LAST_LNG, claims.lastLng)
        }
        val jws = JsonWebSignature().apply {
            payload = jwt.toJson()
            key = this@Jose4jJwtSigner.key
            algorithmHeaderValue = AlgorithmIdentifiers.HMAC_SHA256
            keyIdHeaderValue = active.id
        }
        return jws.compactSerialization
    }

    override fun verify(token: String): TokenClaims {
        // Pick the key by `kid` rather than trying each in turn. Trying
        // them all would make "signed by a key we retired" and "forged"
        // produce the same outcome, and it would also mean the cost of
        // verification grew with the number of keys — which an attacker
        // controls by sending garbage.
        val verificationKey = keyFor(token)

        val consumer = JwtConsumerBuilder()
            .setRequireExpirationTime()
            .setRequireSubject()
            // iat bounds the token's lifetime from the front: without it
            // a token has an expiry but no beginning, and there is nothing
            // to compare a revocation timestamp against.
            .setRequireIssuedAt()
            // iss/aud pin the token to this platform. Without them any
            // HMAC key this service ever held (retired or active) could
            // mint tokens for ANY purpose, and a token signed for some
            // other audience by the same key would verify here.
            .setExpectedIssuer(ISSUER)
            .setExpectedAudience(AUDIENCE)
            .setVerificationKey(verificationKey)
            .setJwsAlgorithmConstraints(
                org.jose4j.jwa.AlgorithmConstraints(
                    org.jose4j.jwa.AlgorithmConstraints.ConstraintType.PERMIT,
                    AlgorithmIdentifiers.HMAC_SHA256,
                )
            )
            .setAllowedClockSkewInSeconds(30)
            .build()

        val claims = try {
            consumer.processToClaims(token)
        } catch (e: InvalidJwtException) {
            throw AuthError.TokenInvalid(e.message ?: "invalid token")
        }

        val userId = try {
            UUID.fromString(claims.subject)
        } catch (e: IllegalArgumentException) {
            throw AuthError.TokenInvalid("sub is not a UUID")
        }

        // Required, not optional. A token without a project claim predates
        // multi-tenancy and must not be honoured: accepting one would mean
        // guessing which tenant its bearer belongs to, and the safe guess
        // does not exist.
        val projectIdStr = claims.getClaimValue(CLAIM_PROJECT_ID, String::class.java)
            ?: throw AuthError.TokenInvalid("missing $CLAIM_PROJECT_ID claim")
        val projectId = try {
            UUID.fromString(projectIdStr)
        } catch (e: IllegalArgumentException) {
            throw AuthError.TokenInvalid("$CLAIM_PROJECT_ID is not a UUID")
        }

        val sessionIdStr = claims.getClaimValue(CLAIM_SESSION_ID, String::class.java)
            ?: throw AuthError.TokenInvalid("missing $CLAIM_SESSION_ID claim")
        val sessionId = try {
            UUID.fromString(sessionIdStr)
        } catch (e: IllegalArgumentException) {
            throw AuthError.TokenInvalid("$CLAIM_SESSION_ID is not a UUID")
        }

        return TokenClaims(
            userId = userId,
            projectId = projectId,
            sessionId = sessionId,
            issuedAt = Instant.ofEpochSecond(claims.issuedAt.value),
            expiresAt = Instant.ofEpochSecond(claims.expirationTime.value),
            lastLat = claims.getClaimValue(CLAIM_LAST_LAT, java.lang.Double::class.java)?.toDouble(),
            lastLng = claims.getClaimValue(CLAIM_LAST_LNG, java.lang.Double::class.java)?.toDouble(),
        )
    }

    /**
     * Resolve the key a token names.
     *
     * A token with NO `kid` predates rotation support and is verified
     * against the active key — the only key that could have signed it,
     * since retired keys are only ever introduced alongside a kid. That
     * allowance exists so deploying this change does not log everyone
     * out, and it costs nothing: it accepts only keys we already hold.
     */
    private fun keyFor(token: String): HmacKey {
        val kid = try {
            JsonWebSignature().apply { compactSerialization = token }.keyIdHeaderValue
        } catch (e: org.jose4j.lang.JoseException) {
            throw AuthError.TokenInvalid("malformed token")
        }
        if (kid == null) return keysById.getValue(active.id)
        return keysById[kid] ?: throw AuthError.TokenInvalid("unknown signing key")
    }

    companion object {
        /**
         * Used when a deployment names no key. Rotation still works from
         * here: the first rotation introduces a new id and retires this
         * one.
         */
        const val DEFAULT_KEY_ID = "k1"

        const val CLAIM_PROJECT_ID = "atlas:project_id"
        const val CLAIM_SESSION_ID = "atlas:session_id"
        const val CLAIM_LAST_LAT = "atlas:last_lat"
        const val CLAIM_LAST_LNG = "atlas:last_lng"
    }
}
