package tj.payment.core

import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.PublicKey
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import java.security.spec.X509EncodedKeySpec
import java.util.Base64
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The signed payload is a contract with the server (crates/api/src/devices.rs).
 * These vectors are byte-for-byte the ones in crates/api/tests/devices.rs: the
 * same payloads, the server's deterministic (RFC 6979) signatures verified here
 * with java.security, and java.security signatures the server verifies there.
 * Key: the RFC 6979 A.2.5 P-256 test key.
 */
class DeviceSignatureVectorsTest {

    private val spki =
        "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEYP7UuiVanTHJYet0xjVtaMBJuJI7Yfps5mliLmDyn7Z5A/4QCLi8maQa6elWKLxk8vGyDC1+n1F3o8KU1EYimQ=="

    private class Vector(val payment: PendingPayment, val payload: String, val rustSig: String, val javaSig: String)

    private val user = "11111111-1111-4111-8111-111111111111"

    private val vectors = listOf(
        Vector(
            PendingPayment(
                idempotencyKey = "22222222-2222-4222-8222-222222222222",
                fromAccount = "33333333-3333-4333-8333-333333333333",
                toAccount = "44444444-4444-4444-8444-444444444444",
                amountMinor = 2500,
                currency = "TJS",
                recipientLabel = "992900000001",
                userId = user,
                kind = PaymentKind.TRANSFER,
            ),
            "23:tj.payment.authorize.v1;36:11111111-1111-4111-8111-111111111111;" +
                "8:transfer;36:22222222-2222-4222-8222-222222222222;" +
                "36:33333333-3333-4333-8333-333333333333;" +
                "36:44444444-4444-4444-8444-444444444444;4:2500;3:TJS;0:;",
            "MEUCIHE0pzWUCRslKd4m3rTsEz0OVTBWUIHCiMj90gcoVUJpAiEAh25kQ4JWz+HW2tYB0Zs7af8jNOzbBExJAZTgsSNbUCk=",
            "MEUCIQDkg2FM4TvYKgF9B8O4M3VZ/ddp5znKq7Eqxr9QjmGJdAIgF3rq4LjeG5ocb9l3Jk9Ingl/r1t6DfjBIZNGo9IaaOk=",
        ),
        Vector(
            PendingPayment(
                idempotencyKey = "55555555-5555-4555-8555-555555555555",
                fromAccount = "66666666-6666-4666-8666-666666666666",
                toAccount = "33333333-3333-4333-8333-333333333333",
                amountMinor = 10000,
                currency = "USD",
                recipientLabel = "TJS",
                userId = user,
                kind = PaymentKind.FX,
            ),
            "23:tj.payment.authorize.v1;36:11111111-1111-4111-8111-111111111111;" +
                "2:fx;36:55555555-5555-4555-8555-555555555555;" +
                "36:66666666-6666-4666-8666-666666666666;" +
                "36:33333333-3333-4333-8333-333333333333;5:10000;3:USD;0:;",
            "MEQCICoMDyzygwLaw+NYAWZfnhRkVRso2Xwb/T90i2wCWTqTAiBofhLuQ8f6/dzshRJx1xt0hshd6G9YTyY+r+jm67alkw==",
            "MEUCIQDoYfQvdZE7gfzMxCo2tQlyDOHnKXrtWYdTi+KqNk+EYQIgQxyie5uWHyWdAcZooeGZv/rTFi0SZVtg+Q5MNxjf7yM=",
        ),
        Vector(
            PendingPayment(
                idempotencyKey = "77777777-7777-4777-8777-777777777777",
                fromAccount = "33333333-3333-4333-8333-333333333333",
                toAccount = "88888888-8888-4888-8888-888888888888",
                amountMinor = 1500,
                currency = "TJS",
                recipientLabel = "Nan Bakery",
                checkId = "99999999-9999-4999-8999-999999999999",
                userId = user,
            ),
            "23:tj.payment.authorize.v1;36:11111111-1111-4111-8111-111111111111;" +
                "5:check;36:77777777-7777-4777-8777-777777777777;" +
                "36:33333333-3333-4333-8333-333333333333;" +
                "36:88888888-8888-4888-8888-888888888888;4:1500;3:TJS;" +
                "36:99999999-9999-4999-8999-999999999999;",
            "MEUCIDiBaX59vCc/7zc1hXOG5dEaHI6EBizjhPeDzAuiWY3iAiEAzw4/f+4/U8iA2ejLXNB2euBDJYrpQ/DaNALfN8lSjvA=",
            "MEQCIGzE5Q+nKnrOpIHZMHBc9pyzNIkhYTEL1bO+iJm259hJAiAS4UqGCVYoBVxic7XK04HPFqW9oX5uGcN6OnOIVqVjAw==",
        ),
    )

    private fun publicKey(b64: String): PublicKey =
        KeyFactory.getInstance("EC").generatePublic(X509EncodedKeySpec(Base64.getDecoder().decode(b64)))

    /** What the server does: SHA256withECDSA over the payload, DER signature. */
    private fun verifies(key: PublicKey, payload: ByteArray, signatureB64: String): Boolean =
        Signature.getInstance("SHA256withECDSA").run {
            initVerify(key)
            update(payload)
            verify(Base64.getDecoder().decode(signatureB64))
        }

    @Test
    fun `the payload is byte for byte the server's canonical payload`() {
        for (v in vectors) {
            assertEquals(v.payload, String(v.payment.authorizationPayload(), Charsets.UTF_8))
            assertArrayEquals(v.payload.toByteArray(Charsets.UTF_8), v.payment.authorizationPayload())
        }
    }

    @Test
    fun `server-made and keystore-style signatures verify over the shared vectors`() {
        val key = publicKey(spki)
        for (v in vectors) {
            val payload = v.payment.authorizationPayload()
            assertTrue("server signature: ${v.payload}", verifies(key, payload, v.rustSig))
            assertTrue("java.security signature: ${v.payload}", verifies(key, payload, v.javaSig))
            // Any change to what the user approved breaks it.
            val tampered = v.payment.copy(amountMinor = v.payment.amountMinor + 1).authorizationPayload()
            assertFalse(verifies(key, tampered, v.javaSig))
        }
    }

    @Test
    fun `a fresh P-256 key's export is what POST v1 devices takes and its signatures verify`() {
        // The Keystore key is an EC P-256 key whose public half exports as X.509
        // SubjectPublicKeyInfo DER — the format the server parses.
        val pair = KeyPairGenerator.getInstance("EC").apply { initialize(ECGenParameterSpec("secp256r1")) }.generateKeyPair()
        val exported = Base64.getEncoder().encodeToString(pair.public.encoded)
        val payload = vectors[0].payment.authorizationPayload()
        val signature = Signature.getInstance("SHA256withECDSA").run {
            initSign(pair.private)
            update(payload)
            Base64.getEncoder().encodeToString(sign())
        }
        assertTrue(verifies(publicKey(exported), payload, signature))
        assertEquals(91, pair.public.encoded.size) // uncompressed point, like the server's canonical form
    }

    @Test
    fun `ids are signed lowercase, as the server prints them`() {
        val v = vectors[2]
        val shouting = v.payment.copy(
            userId = v.payment.userId.uppercase(),
            idempotencyKey = v.payment.idempotencyKey.uppercase(),
            fromAccount = v.payment.fromAccount.uppercase(),
            toAccount = v.payment.toAccount.uppercase(),
            checkId = v.payment.checkId?.uppercase(),
        )
        assertArrayEquals(v.payment.authorizationPayload(), shouting.authorizationPayload())
    }

    @Test
    fun `lengths count UTF-8 bytes`() {
        val p = vectors[0].payment.copy(currency = "сом")
        assertTrue(String(p.authorizationPayload(), Charsets.UTF_8).contains(";6:сом;"))
    }

    @Test
    fun `the stored device id and signature are what every attempt sends`() {
        val p = vectors[0].payment
        assertEquals(emptyMap<String, String>(), p.deviceHeaders())
        assertEquals(emptyMap<String, String>(), p.copy(authorization = "c2ln").deviceHeaders())
        assertEquals(
            mapOf("X-Device-Id" to "d1", "X-Device-Signature" to "c2ln"),
            p.copy(authorization = "c2ln", deviceId = "d1").deviceHeaders(),
        )
    }
}
