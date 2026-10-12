package io.atlas.auth.cache

import com.github.benmanes.caffeine.cache.Cache
import com.github.benmanes.caffeine.cache.Caffeine
import io.atlas.auth.core.TokenClaims
import java.time.Duration

/**
 * In-process cache of token validation results so the gateway-hot path does
 * not hit Postgres on every request. Key is the JWT signature segment (the
 * portion after the last '.') because that is the cheapest globally-unique
 * identifier for a token without storing the raw token itself.
 *
 * 30s TTL: short enough that natural expiry handles most revocations even
 * before the Kafka invalidation broadcast arrives, long enough to absorb
 * realistic gateway QPS. The Kafka consumer in [io.atlas.auth.kafka]
 * proactively evicts entries when other instances revoke.
 *

 */
class TokenValidationCache(
    ttl: Duration = Duration.ofSeconds(30),
    maximumSize: Long = 100_000,
) {
    private val cache: Cache<String, TokenClaims> = Caffeine.newBuilder()
        .expireAfterWrite(ttl)
        .maximumSize(maximumSize)
        .build()

    fun get(token: String): TokenClaims? = cache.getIfPresent(signatureSegment(token))

    fun put(token: String, claims: TokenClaims) {
        cache.put(signatureSegment(token), claims)
    }

    /**
     * Evicts by signature segment. Used by the Kafka consumer when it receives
     * a REVOKED event from another instance. The event carries the SHA-256 of
     * the full token, not just the signature, so callers eviction-by-hash use
     * [evictByTokenHash] below.
     */
    fun evict(token: String) {
        cache.invalidate(signatureSegment(token))
    }

    /**
     * Evicts every cached entry whose token hash matches. Used by the cache-
     * invalidation consumer which only knows the token hash, not the raw
     * token. O(n) over the live cache, which is acceptable at the 30s/100k
     * scale this cache targets.
     */
    fun evictByTokenHash(tokenHash: String) {
        cache.asMap().entries.removeIf { entry ->
            // Recompute the hash from a stand-in: we don't have the raw token,
            // so we re-derive from the cache key + claims if needed. In
            // practice, the producer side stores both the sig segment (as
            // cache key) AND the hash side-channel; we therefore filter by
            // looking up the hash that was set at put-time.
            // We keep an auxiliary index for this.
            hashIndex[entry.key] == tokenHash
        }
        hashIndex.entries.removeIf { it.value == tokenHash }
    }

    /**
     * Drop every cached validation for one user.
     *
     * Needed by password reset, which revokes all of a user's sessions at
     * once. Revocation is recorded in Postgres, but ValidateToken answers
     * from this cache first — so without this a token issued before the
     * reset keeps validating from memory for the rest of the TTL, which is
     * precisely the window the reset exists to close.
     *
     * O(n) over the live cache, like [evictByTokenHash], and for the same
     * reason: it happens once per reset, not once per request.
     */
    fun evictUser(userId: java.util.UUID) {
        val removedKeys = cache.asMap().entries
            .filter { it.value.userId == userId }
            .map { it.key }
        removedKeys.forEach { cache.invalidate(it) }
        hashIndex.keys.removeAll(removedKeys.toSet())
    }

    fun putWithHash(token: String, claims: TokenClaims, tokenHash: String) {
        val key = signatureSegment(token)
        cache.put(key, claims)
        hashIndex[key] = tokenHash
    }

    private val hashIndex = java.util.concurrent.ConcurrentHashMap<String, String>()

    fun size(): Long = cache.estimatedSize()

    companion object {
        /** Returns the third segment of a JWT (header.payload.signature). */
        fun signatureSegment(token: String): String {
            val lastDot = token.lastIndexOf('.')
            return if (lastDot >= 0 && lastDot < token.length - 1) {
                token.substring(lastDot + 1)
            } else {
                // Malformed token: use the whole string as the key. Verification
                // will reject it anyway, so we don't try to validate here.
                token
            }
        }
    }
}
