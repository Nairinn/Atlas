package io.atlas.payments.core

import java.security.MessageDigest

/** Lowercase hex SHA-256 of [input]. */
fun sha256Hex(input: String): String {
    val digest = MessageDigest.getInstance("SHA-256").digest(input.toByteArray(Charsets.UTF_8))
    val sb = StringBuilder(digest.size * 2)
    for (b in digest) {
        sb.append(Character.forDigit((b.toInt() shr 4) and 0xF, 16))
        sb.append(Character.forDigit(b.toInt() and 0xF, 16))
    }
    return sb.toString()
}

/**
 * Hash of the business arguments behind an idempotency key. Stored on the
 * transaction row so a reused key with DIFFERENT args is rejected rather than
 * silently returning the wrong transaction (see 0031_payments_outbox.sql).
 *
 * v2 includes the application fee: two initiations with the same key but
 * different fees are different requests, and must conflict rather than
 * replay. The `v2:` prefix lets stored v1 hashes stay readable.
 */
fun idempotencyArgsHash(
    fromUserId: String,
    toUserId: String,
    amountCents: Long,
    rideId: String,
    applicationFeeCents: Long = 0,
): String = sha256Hex("v2:$fromUserId|$toUserId|$amountCents|$rideId|$applicationFeeCents")

/** True when [stored] matches [args] under either hash version. */
fun idempotencyArgsMatch(
    stored: String?,
    fromUserId: String,
    toUserId: String,
    amountCents: Long,
    rideId: String,
    applicationFeeCents: Long,
): Boolean {
    if (stored == null) return false
    if (stored == idempotencyArgsHash(fromUserId, toUserId, amountCents, rideId, applicationFeeCents)) return true
    // v1 rows predate the fee field; they only ever stored fee-less
    // transfers, so the legacy hash is accepted only for a zero fee.
    if (applicationFeeCents == 0L) {
        return stored == sha256Hex("$fromUserId|$toUserId|$amountCents|$rideId")
    }
    return false
}
