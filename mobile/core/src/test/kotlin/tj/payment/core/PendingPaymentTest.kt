package tj.payment.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** A check payment rides the same persisted-key machine as a transfer; the check id must survive the store. */
class PendingPaymentTest {

    private val json = Json { ignoreUnknownKeys = true }

    private class MemoryStore : PendingPaymentStore {
        var stored: PendingPayment? = null
        var savedBeforeNetwork = false
        override fun load(): PendingPayment? = stored
        override fun save(payment: PendingPayment): Boolean {
            stored = payment
            return true
        }
        override fun clear() {
            stored = null
        }
    }

    @Test
    fun `check id round-trips through JSON and is absent on legacy records`() {
        val p = PendingPayment("k1", "from", "to", 200, "TJS", "Nan Bakery", checkId = "c1")
        val encoded = json.encodeToString(PendingPayment.serializer(), p)
        assertTrue(encoded, encoded.contains("\"check_id\":\"c1\""))
        assertEquals(p, json.decodeFromString(PendingPayment.serializer(), encoded))

        val legacy = """{"idempotency_key":"k","from_account":"a","to_account":"b","amount_minor":1,""" +
            """"currency":"TJS","recipient_label":"x"}"""
        assertNull(json.decodeFromString(PendingPayment.serializer(), legacy).checkId)
    }

    @Test
    fun `submitNew persists the check id before the network sees it`() = runBlocking {
        val store = MemoryStore()
        var seen: PendingPayment? = null
        val submitter = PaymentSubmitter(
            store = store,
            newKey = { "key-1" },
            transfer = { p ->
                store.savedBeforeNetwork = store.stored?.checkId == "c1"
                seen = p
                ApiOutcome.Ok(PostResponse("key-1"))
            },
            io = Dispatchers.Unconfined,
        )
        val result = submitter.submitNew("w1", "merchant-wallet", 200, "TJS", "Nan Bakery", checkId = "c1")
        assertEquals(SubmitResult.Posted("key-1", alreadyPosted = false), result)
        assertNotNull(seen)
        assertEquals("c1", seen?.checkId)
        assertTrue("key + check id must be on disk before the first attempt", store.savedBeforeNetwork)
        assertNull("settled: record cleared", store.stored)
    }
}
