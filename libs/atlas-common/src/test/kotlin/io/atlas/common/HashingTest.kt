package io.atlas.common

import org.junit.jupiter.api.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotEquals

class HashingTest {

    @Test
    fun `sha256Hex matches known vectors`() {
        // RFC 6234 / NIST FIPS 180-4 test vectors.
        assertEquals(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sha256Hex(""),
        )
        assertEquals(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            sha256Hex("abc"),
        )
    }

    @Test
    fun `different inputs hash differently`() {
        assertNotEquals(sha256Hex("a"), sha256Hex("b"))
    }
}
