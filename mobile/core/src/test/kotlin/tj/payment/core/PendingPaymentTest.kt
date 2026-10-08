package tj.payment.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The persisted record: its wire shape (incl. records written by older app
 * versions), the per-user store over a key-value port, and the approval payload.
 */
class PendingPaymentTest {

    private val json = Json { ignoreUnknownKeys = true }

    /** In-memory [SecureKeyValue]; [faulty] simulates a Keystore fault. */
    private class MemoryKv : SecureKeyValue {
        val map = linkedMapOf<String, String>()
        val flags = linkedMapOf<String, Boolean>()
        var persists = true
        var faulty = false
        private fun check() {
            if (faulty) throw LocalStorageException("keystore down")
        }
        override fun get(key: String): String? = check().let { map[key] }
        override fun put(key: String, value: String): Boolean {
            check()
            if (!persists) return false
            map[key] = value
            return true
        }
        override fun remove(key: String): Boolean = check().let { map.remove(key); flags.remove(key); true }
        override fun getFlag(key: String): Boolean = check().let { flags[key] == true }
        override fun putFlag(key: String, value: Boolean): Boolean = check().let { flags[key] = value; true }
    }

    private fun payment(key: String = "k1", user: String = "alice", amount: Long = 200) =
        PendingPayment(key, "from", "to", amount, "TJS", "Nan Bakery", userId = user, authorization = "sig")

    // --- wire shape ---

    @Test
    fun `check id, owner, kind and approval round-trip through JSON`() {
        val p = PendingPayment(
            "k1", "from", "to", 200, "TJS", "Nan Bakery", checkId = "c1",
            userId = "u1", authorization = "c2ln", createdAtMs = 5,
        )
        assertEquals(PaymentKind.CHECK, p.kind)
        val encoded = json.encodeToString(PendingPayment.serializer(), p)
        assertEquals(p, json.decodeFromString(PendingPayment.serializer(), encoded))
    }

    @Test
    fun `a legacy record decodes with no owner and the kind its check id implies`() {
        val legacy = """{"idempotency_key":"k","from_account":"a","to_account":"b","amount_minor":1,""" +
            """"currency":"TJS","recipient_label":"x"}"""
        val decoded = json.decodeFromString(PendingPayment.serializer(), legacy)
        assertNull(decoded.checkId)
        assertEquals("", decoded.userId)
        assertEquals(PaymentKind.TRANSFER, decoded.kind)
        assertNull(decoded.authorization)

        val legacyCheck = legacy.dropLast(1) + ""","check_id":"c9"}"""
        assertEquals(PaymentKind.CHECK, json.decodeFromString(PendingPayment.serializer(), legacyCheck).kind)
    }

    @Test
    fun `the approval payload pins every money-relevant field and nothing else`() {
        val base = payment()
        val payload = base.authorizationPayload()
        for (changed in listOf(
            base.copy(idempotencyKey = "k2"),
            base.copy(userId = "bob"),
            base.copy(fromAccount = "other"),
            base.copy(toAccount = "other"),
            base.copy(amountMinor = 201),
            base.copy(currency = "USD"),
            base.copy(checkId = "c1"),
            base.copy(kind = PaymentKind.FX),
        )) {
            assertFalse("payload must change for $changed", payload.contentEquals(changed.authorizationPayload()))
        }
        // Display-only fields and the signature itself are not part of what is signed.
        assertTrue(payload.contentEquals(base.copy(recipientLabel = "renamed", authorization = null, createdAtMs = 9).authorizationPayload()))
        // Length-prefixed: shifting characters between fields cannot collide.
        assertFalse(
            base.copy(fromAccount = "ab", toAccount = "c").authorizationPayload()
                .contentEquals(base.copy(fromAccount = "a", toAccount = "bc").authorizationPayload()),
        )
    }

    // --- the per-user key-value store ---

    @Test
    fun `records are kept per user and never visible across users`() {
        val kv = MemoryKv()
        val store = KeyValuePendingPaymentStore(kv)
        assertTrue(store.save(payment("ka", "alice")))
        assertTrue(store.save(payment("kb", "bob")))

        assertEquals("ka", store.load("alice")?.idempotencyKey)
        assertEquals("kb", store.load("bob")?.idempotencyKey)
        assertNull(store.load("carol"))

        store.clear("alice", "ka")
        assertNull(store.load("alice"))
        assertEquals("kb", store.load("bob")?.idempotencyKey)
    }

    @Test
    fun `clear never removes a newer record that replaced the settled one`() {
        val store = KeyValuePendingPaymentStore(MemoryKv())
        store.save(payment("new", "alice"))
        store.clear("alice", "old")
        assertEquals("new", store.load("alice")?.idempotencyKey)
    }

    @Test
    fun `a legacy single record is adopted by the first user who loads it`() {
        val kv = MemoryKv()
        kv.map[KeyValuePendingPaymentStore.LEGACY_KEY] =
            """{"idempotency_key":"old","from_account":"a","to_account":"b","amount_minor":5,"currency":"TJS","recipient_label":"x"}"""
        val store = KeyValuePendingPaymentStore(kv)

        val adopted = store.load("alice")
        assertEquals("old", adopted?.idempotencyKey)
        assertEquals("alice", adopted?.userId)
        assertNull("moved out of the legacy slot", kv.map[KeyValuePendingPaymentStore.LEGACY_KEY])
        assertNull("not shown to anyone else afterwards", store.load("bob"))
        assertEquals("old", store.load("alice")?.idempotencyKey)
    }

    @Test
    fun `a settled payment is never re-adopted from a leftover legacy copy`() {
        val kv = MemoryKv()
        val legacyJson =
            """{"idempotency_key":"old","from_account":"a","to_account":"b","amount_minor":5,"currency":"TJS","recipient_label":"x"}"""
        val store = KeyValuePendingPaymentStore(kv)
        // An adoption interrupted after writing alice's slot, before removing the legacy copy.
        kv.map[KeyValuePendingPaymentStore.LEGACY_KEY] = legacyJson
        store.save(payment("old", "alice"))

        store.clear("alice", "old")
        assertNull(store.load("alice"))
        assertNull(kv.map[KeyValuePendingPaymentStore.LEGACY_KEY])

        // A legacy record with a DIFFERENT key is not touched by clearing another payment.
        kv.map[KeyValuePendingPaymentStore.LEGACY_KEY] = legacyJson
        store.save(payment("new", "bob"))
        store.clear("bob", "new")
        assertEquals(legacyJson, kv.map[KeyValuePendingPaymentStore.LEGACY_KEY])
    }

    @Test
    fun `an unreadable record is dropped once with a one-time notice`() {
        val kv = MemoryKv()
        kv.map[KeyValuePendingPaymentStore.keyFor("alice")] = "{not json"
        val store = KeyValuePendingPaymentStore(kv)

        assertNull(store.load("alice"))
        assertTrue(store.consumeUnreadableRecordNotice())
        assertFalse("only once", store.consumeUnreadableRecordNotice())
        assertTrue(store.save(payment()))
    }

    @Test
    fun `a storage fault surfaces as LocalStorageException, never as nothing pending`() {
        val kv = MemoryKv()
        val store = KeyValuePendingPaymentStore(kv)
        store.save(payment())
        kv.faulty = true
        try {
            store.load("alice")
            throw AssertionError("expected LocalStorageException")
        } catch (_: LocalStorageException) {
            // expected
        }
    }

    @Test
    fun `submitNew persists the check id before the network sees it`() = runBlocking {
        val store = KeyValuePendingPaymentStore(MemoryKv())
        var storedAtAttempt: PendingPayment? = null
        val submitter = PaymentSubmitter(
            store = store,
            currentUser = { "alice" },
            newKey = { "key-1" },
            authorizer = { Authorization.Granted("sig") },
            execute = { p ->
                storedAtAttempt = store.load("alice")
                ApiOutcome.Ok(Settlement(p.idempotencyKey, alreadyPosted = false))
            },
            lookup = { error("not used") },
            void = { error("not used") },
            io = Dispatchers.Unconfined,
        )
        val result = submitter.submitNew("w1", "merchant-wallet", 200, "TJS", "Nan Bakery", checkId = "c1")
        assertEquals(SubmitResult.Posted("key-1", alreadyPosted = false), result)
        assertNotNull(storedAtAttempt)
        assertEquals("c1", storedAtAttempt?.checkId)
        assertEquals(PaymentKind.CHECK, storedAtAttempt?.kind)
        assertNull("settled: record cleared", store.load("alice"))
    }
}
