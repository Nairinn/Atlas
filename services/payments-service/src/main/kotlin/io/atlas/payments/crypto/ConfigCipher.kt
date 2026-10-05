package io.atlas.payments.crypto

import java.security.SecureRandom
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * AES-256-GCM for per-project processor credentials at rest.
 *
 * `control.project_payment_config` stores a tenant's live Stripe secret
 * key — a credential that can move real money. The database is exactly
 * the artifact that leaks (dumps, replicas, backups, an SQL console left
 * open), so the column is ciphertext and the key lives in an environment
 * variable the database never sees.
 *
 * The env var is 32 bytes, base64- or hex-encoded (both accepted: ops
 * hand-copy these between secret stores and both encodings show up in
 * the wild). A missing key is a startup failure, not a silent no-op: an
 * encryption layer that quietly passes values through would leave every
 * credential in plaintext with nothing in the logs saying so.
 *
 * GCM rather than bare CBC: it is authenticated, so a tampered row
 * fails to decrypt rather than decrypting to a corrupted key that gets
 * sent to Stripe as a credential.
 */
class ConfigCipher(keyEncoded: String) {

    private val key: ByteArray = decode(keyEncoded)

    init {
        require(key.size == 32) {
            "PAYMENT_CONFIG_ENC_KEY must decode to 32 bytes (AES-256); got ${key.size}"
        }
    }

    /** Random per-encryption nonce; GCM's contract is that it never repeats under one key. */
    private val random = SecureRandom()

    fun encrypt(plaintext: String): ByteArray {
        val nonce = ByteArray(NONCE_LEN).also { random.nextBytes(it) }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BITS, nonce))
        return nonce + cipher.doFinal(plaintext.toByteArray(Charsets.UTF_8))
    }

    fun decrypt(ciphertext: ByteArray): String {
        require(ciphertext.size > NONCE_LEN) { "ciphertext too short" }
        val nonce = ciphertext.copyOfRange(0, NONCE_LEN)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BITS, nonce))
        return cipher.doFinal(ciphertext, NONCE_LEN, ciphertext.size - NONCE_LEN)
            .toString(Charsets.UTF_8)
    }

    private companion object {
        private const val NONCE_LEN = 12
        private const val TAG_BITS = 128

        private fun decode(encoded: String): ByteArray {
            // Hex is unambiguous and base64 is conventional; 64 hex chars
            // cannot be valid base64 of 32 bytes (that is 44 chars with
            // padding), so length disambiguates without a try/catch race.
            val trimmed = encoded.trim()
            return if (trimmed.length == 64 && trimmed.all { it.isDigit() || it in 'a'..'f' || it in 'A'..'F' }) {
                ByteArray(32) { i ->
                    ((Character.digit(trimmed[i * 2], 16) shl 4) or Character.digit(trimmed[i * 2 + 1], 16)).toByte()
                }
            } else {
                Base64.getDecoder().decode(trimmed)
            }
        }
    }
}
