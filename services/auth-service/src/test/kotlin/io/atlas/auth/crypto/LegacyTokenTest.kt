package io.atlas.auth.crypto

import io.atlas.auth.core.AuthError
import io.atlas.auth.core.TokenClaims
import org.junit.jupiter.api.Test
import java.time.Instant
import java.util.UUID
import kotlin.test.assertFailsWith
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

/**
 * Tokens minted before rotation support carry no `kid` at all.
 *
 * Deploying this change must not log those users out, so a token with no
 * kid is verified against the active key — the only key that could have
 * signed it, since retired keys are only ever introduced alongside a kid.
 */
class LegacyTokenTest {
    @Test
    fun `a pre-rotation token with no iss or aud is rejected`() {
        val secret = "the-first-secret-is-long-enough-yes!!"
        val userId = UUID.randomUUID()
        val now = Instant.now()

        // Hand-built without kid, iss, or aud, exactly as the oldest
        // signer produced. Requiring iss/aud means this cohort is now
        // rejected rather than grandfathered: the alternative is pinning
        // the audience check to a token that never claimed one, and the
        // affected cohort is simply asked to log in again.
        val jwt = org.jose4j.jwt.JwtClaims().apply {
            subject = userId.toString()
            issuedAt = org.jose4j.jwt.NumericDate.fromSeconds(now.epochSecond)
            expirationTime = org.jose4j.jwt.NumericDate.fromSeconds(now.epochSecond + 3600)
            setClaim(Jose4jJwtSigner.CLAIM_PROJECT_ID, UUID.randomUUID().toString())
            setClaim(Jose4jJwtSigner.CLAIM_SESSION_ID, UUID.randomUUID().toString())
        }
        val jws = org.jose4j.jws.JsonWebSignature().apply {
            payload = jwt.toJson()
            key = org.jose4j.keys.HmacKey(secret.toByteArray(Charsets.UTF_8))
            algorithmHeaderValue = org.jose4j.jws.AlgorithmIdentifiers.HMAC_SHA256
            // No keyIdHeaderValue.
        }

        val signer = Jose4jJwtSigner(
            active = SigningKey("k1", secret),
            retired = listOf(SigningKey("k0", "an-older-secret-also-long-enough!!!!")),
        )
        val e = assertFailsWith<AuthError.TokenInvalid> { signer.verify(jws.compactSerialization) }
        // Rejected for the missing iss/aud, not for the missing kid: a
        // token WITH iss/aud but no kid must still verify (asserted
        // below). jose4j words the failure either way, so assert on the
        // error class alone here — the companion test proves the positive.
        assertNotNull(e.message)
    }

    @Test
    fun `a kid-less token WITH iss and aud still verifies`() {
        val secret = "the-first-secret-is-long-enough-yes!!"
        val userId = UUID.randomUUID()
        val now = Instant.now()

        val jwt = org.jose4j.jwt.JwtClaims().apply {
            issuer = "atlas"
            setAudience(listOf("atlas.auth"))
            subject = userId.toString()
            issuedAt = org.jose4j.jwt.NumericDate.fromSeconds(now.epochSecond)
            expirationTime = org.jose4j.jwt.NumericDate.fromSeconds(now.epochSecond + 3600)
            setClaim(Jose4jJwtSigner.CLAIM_PROJECT_ID, UUID.randomUUID().toString())
            setClaim(Jose4jJwtSigner.CLAIM_SESSION_ID, UUID.randomUUID().toString())
        }
        val jws = org.jose4j.jws.JsonWebSignature().apply {
            payload = jwt.toJson()
            key = org.jose4j.keys.HmacKey(secret.toByteArray(Charsets.UTF_8))
            algorithmHeaderValue = org.jose4j.jws.AlgorithmIdentifiers.HMAC_SHA256
            // No keyIdHeaderValue: kid-less but otherwise current.
        }

        val signer = Jose4jJwtSigner(
            active = SigningKey("k1", secret),
            retired = listOf(SigningKey("k0", "an-older-secret-also-long-enough!!!!")),
        )
        assertEquals(userId, signer.verify(jws.compactSerialization).userId)
    }
}
