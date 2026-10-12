package io.atlas.common

import java.security.MessageDigest

/**
 * SHA-256 hex digest. Used for idempotency-key hashing and for putting a
 * JWT on the wire as a digest rather than the raw token: anything logged
 * or published learns *which* token changed state without being able to
 * use it.
 */
fun sha256Hex(input: String): String {
    val digest = MessageDigest.getInstance("SHA-256").digest(input.toByteArray(Charsets.UTF_8))
    return buildString(digest.size * 2) {
        for (b in digest) {
            val v = b.toInt() and 0xFF
            append(HEX[v ushr 4])
            append(HEX[v and 0x0F])
        }
    }
}

private val HEX = "0123456789abcdef".toCharArray()
