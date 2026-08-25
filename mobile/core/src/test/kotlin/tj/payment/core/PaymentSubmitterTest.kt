package tj.payment.core

import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The idempotency state machine is where a client-side money bug would hide:
 * a lost key can double-charge, a dropped key can lose a payment's outcome.
 * Every transition is pinned here, on the JVM, with fakes.
 */
class PaymentSubmitterTest {

    private class FakeStore : PendingPaymentStore {
        var stored: PendingPayment? = null
        override fun load(): PendingPayment? = stored
        override fun save(payment: PendingPayment) {
            stored = payment
        }
        override fun clear() {
            stored = null
        }
    }

    private fun posted(id: String = "txn-1") =
        ApiOutcome.Ok(PostResponse(transactionId = id, status = "posted"))

    private fun submitter(
        store: FakeStore,
        keys: MutableList<String> = mutableListOf("key-1", "key-2"),
        transfer: suspend (PendingPayment) -> ApiOutcome<PostResponse>,
    ) = PaymentSubmitter(store, { keys.removeAt(0) }, transfer)

    @Test
    fun `success posts once and clears the pending payment`() = runBlocking {
        val store = FakeStore()
        val seen = mutableListOf<PendingPayment>()
        val s = submitter(store) { p ->
            seen += p
            posted()
        }

        val result = s.submitNew("from", "to", 2_500, "TJS", "992900000001")

        assertEquals(SubmitResult.Posted("txn-1", alreadyPosted = false), result)
        assertNull(store.stored)
        assertEquals(listOf("key-1"), seen.map { it.idempotencyKey })
    }

    @Test
    fun `payment is durable before the first network attempt`() = runBlocking {
        val store = FakeStore()
        var storedAtAttemptTime: PendingPayment? = null
        val s = submitter(store) { _ ->
            storedAtAttemptTime = store.load()
            posted()
        }

        s.submitNew("from", "to", 100, "TJS", "x")

        // If the app dies mid-request, the key must already be on disk.
        assertNotNull(storedAtAttemptTime)
        assertEquals("key-1", storedAtAttemptTime?.idempotencyKey)
    }

    @Test
    fun `offline keeps the payment and retry reuses the SAME key`() = runBlocking {
        val store = FakeStore()
        val seenKeys = mutableListOf<String>()
        var failNext = true
        val s = submitter(store) { p ->
            seenKeys += p.idempotencyKey
            if (failNext) ApiOutcome.Offline(RuntimeException("no network"))
            else posted()
        }

        val first = s.submitNew("from", "to", 100, "TJS", "x")
        assertEquals(SubmitResult.Unsettled(offline = true), first)
        assertNotNull(store.stored)

        failNext = false
        val second = s.retryPending()
        assertTrue(second is SubmitResult.Posted)
        assertNull(store.stored)
        assertEquals(listOf("key-1", "key-1"), seenKeys)
    }

    @Test
    fun `rate limited and 5xx keep the key, definitive 4xx clears it`() = runBlocking {
        for ((outcome, kept) in listOf(
            ApiOutcome.Failed(ErrorCode.RATE_LIMITED, 429, null) to true,
            ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null) to true,
            ApiOutcome.Failed(ErrorCode.INSUFFICIENT_FUNDS, 422, null) to false,
            ApiOutcome.Failed(ErrorCode.BAD_REQUEST, 400, null) to false,
        )) {
            val store = FakeStore()
            val s = submitter(store) { outcome }
            val result = s.submitNew("from", "to", 100, "TJS", "x")
            if (kept) {
                assertEquals(SubmitResult.Unsettled(offline = false), result)
                assertNotNull("expected key kept for $outcome", store.stored)
            } else {
                assertTrue("expected Rejected for $outcome", result is SubmitResult.Rejected)
                assertNull("expected key cleared for $outcome", store.stored)
            }
        }
    }

    @Test
    fun `a second submit is refused while one is unsettled`() = runBlocking {
        val store = FakeStore()
        val s = submitter(store) { ApiOutcome.Offline(RuntimeException("down")) }

        assertNotNull(s.submitNew("from", "to", 100, "TJS", "x"))
        // The first payment is unsettled; a new one may not race it.
        assertNull(s.submitNew("from", "to2", 200, "TJS", "y"))
        assertEquals(100L, store.stored?.amountMinor)
    }

    @Test
    fun `app restart resumes the stored payment with its original key`() = runBlocking {
        val store = FakeStore()
        val before = submitter(store) { ApiOutcome.Offline(RuntimeException("down")) }
        before.submitNew("from", "to", 300, "TJS", "992901234567")

        // "Restart": a fresh submitter over the same durable store.
        val seenKeys = mutableListOf<String>()
        val after = PaymentSubmitter(store, { "MUST-NOT-BE-USED" }) { p ->
            seenKeys += p.idempotencyKey
            ApiOutcome.Ok(PostResponse("txn-9", status = "already_posted"))
        }

        assertEquals("992901234567", after.pending()?.recipientLabel)
        val result = after.retryPending()
        assertEquals(SubmitResult.Posted("txn-9", alreadyPosted = true), result)
        assertEquals(listOf("key-1"), seenKeys)
        assertNull(store.stored)
    }

    @Test
    fun `retry with nothing pending is a no-op`() = runBlocking {
        val s = submitter(FakeStore()) { posted() }
        assertNull(s.retryPending())
    }

    @Test
    fun `abandon drops the pending payment`() = runBlocking {
        val store = FakeStore()
        val s = submitter(store) { ApiOutcome.Offline(RuntimeException("down")) }
        s.submitNew("from", "to", 100, "TJS", "x")

        s.abandonPending()
        assertNull(s.pending())
    }
}
