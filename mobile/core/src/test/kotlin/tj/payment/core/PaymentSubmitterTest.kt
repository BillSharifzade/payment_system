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

    private class FakeStore(var persists: Boolean = true) : PendingPaymentStore {
        var stored: PendingPayment? = null
        override fun load(): PendingPayment? = stored
        override fun save(payment: PendingPayment): Boolean {
            if (!persists) return false
            stored = payment
            return true
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
        findPosted: suspend (PendingPayment) -> ApiOutcome<Boolean> = { ApiOutcome.Ok(false) },
        transfer: suspend (PendingPayment) -> ApiOutcome<PostResponse>,
    ) = PaymentSubmitter(store, { keys.removeAt(0) }, transfer, findPosted)

    /** A store holding an unsettled payment from "a previous run". */
    private fun storeWithPending(key: String = "key-1") = FakeStore().apply {
        stored = PendingPayment(key, "from", "to", 100, "TJS", "x")
    }

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
    fun `a persist failure never reaches the network`() = runBlocking {
        val store = FakeStore(persists = false)
        var networkCalls = 0
        val s = submitter(store) { _ ->
            networkCalls++
            posted()
        }

        val result = s.submitNew("from", "to", 100, "TJS", "x")

        assertEquals(SubmitResult.NotStarted, result)
        assertEquals("no request may go out without a durable key", 0, networkCalls)
        assertNull(store.stored)
        // Nothing stored, so the user can simply try again (with a new key).
        store.persists = true
        assertTrue(s.submitNew("from", "to", 100, "TJS", "x") is SubmitResult.Posted)
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
    fun `rate limited, 5xx, retry_later and timeout keep the key, definitive 4xx clears it`() = runBlocking {
        for ((outcome, kept) in listOf(
            ApiOutcome.Failed(ErrorCode.RATE_LIMITED, 429, null) to true,
            ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null) to true,
            // The backend's money endpoints answer 503 retry_later (+Retry-After)
            // and 504 timeout: exactly like any other 5xx, same key on retry.
            ApiOutcome.Failed(ErrorCode.RETRY_LATER, 503, null) to true,
            ApiOutcome.Failed(ErrorCode.TIMEOUT, 504, null) to true,
            ApiOutcome.Failed(ErrorCode.UNKNOWN, 502, null) to true,
            ApiOutcome.Failed(ErrorCode.INSUFFICIENT_FUNDS, 422, null) to false,
            ApiOutcome.Failed(ErrorCode.BAD_REQUEST, 400, null) to false,
            ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, null) to false,
            ApiOutcome.Failed(ErrorCode.KYC_REQUIRED, 403, null) to false,
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
    fun `a 2xx whose body could not be read is Unsettled, never Rejected`() = runBlocking {
        // ApiClient.decode() produces exactly this when a 201 comes back blank
        // or as HTML (proxy, captive portal): the server ACCEPTED the transfer.
        for (status in listOf(200, 201, 204)) {
            val store = FakeStore()
            val s = submitter(store) { ApiOutcome.Failed(ErrorCode.UNKNOWN, status, "malformed response") }
            val result = s.submitNew("from", "to", 100, "TJS", "x")
            assertEquals("status $status", SubmitResult.Unsettled(offline = false), result)
            assertNotNull("key must survive a lost $status answer", store.stored)
        }
    }

    @Test
    fun `on a retry, auth and KYC gate errors are Unsettled and keep the key`() = runBlocking {
        // The gate answers before the idempotency replay, so a 401/403 on the
        // RETRY says nothing about whether the FIRST attempt posted.
        for (outcome in listOf(
            ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, null),
            ApiOutcome.Failed(ErrorCode.FORBIDDEN, 403, null),
            ApiOutcome.Failed(ErrorCode.KYC_REQUIRED, 403, null),
        )) {
            val store = storeWithPending()
            var lookups = 0
            val s = submitter(store, findPosted = { lookups++; ApiOutcome.Ok(false) }) { outcome }
            assertEquals("$outcome", SubmitResult.Unsettled(offline = false), s.retryPending())
            assertNotNull("key must be kept for $outcome", store.stored)
            assertEquals("no statement check needed for a gate error", 0, lookups)
        }
    }

    @Test
    fun `a definitive rejection of a retry is confirmed against the statement`() = runBlocking {
        val refusal = ApiOutcome.Failed(ErrorCode.INSUFFICIENT_FUNDS, 422, "no funds")

        // Statement shows the key: the first attempt DID post -> resolved as sent.
        val postedStore = storeWithPending("key-7")
        val looked = mutableListOf<String>()
        val s1 = submitter(postedStore, findPosted = { p -> looked += p.idempotencyKey; ApiOutcome.Ok(true) }) { refusal }
        assertEquals(SubmitResult.Posted("key-7", alreadyPosted = true), s1.retryPending())
        assertNull(postedStore.stored)
        assertEquals(listOf("key-7"), looked)

        // Statement agrees it never posted: the refusal stands, key cleared.
        val cleanStore = storeWithPending()
        val s2 = submitter(cleanStore, findPosted = { ApiOutcome.Ok(false) }) { refusal }
        assertEquals(SubmitResult.Rejected(ErrorCode.INSUFFICIENT_FUNDS, "no funds"), s2.retryPending())
        assertNull(cleanStore.stored)

        // Statement unreadable: never forget a key on a hunch.
        val unsureStore = storeWithPending()
        val s3 = submitter(unsureStore, findPosted = { ApiOutcome.Offline(RuntimeException("down")) }) { refusal }
        assertEquals(SubmitResult.Unsettled(offline = true), s3.retryPending())
        assertNotNull(unsureStore.stored)
    }

    @Test
    fun `a first-attempt rejection needs no statement check`() = runBlocking {
        val store = FakeStore()
        var lookups = 0
        val s = submitter(store, findPosted = { lookups++; ApiOutcome.Ok(true) }) {
            ApiOutcome.Failed(ErrorCode.INSUFFICIENT_FUNDS, 422, null)
        }
        assertTrue(s.submitNew("from", "to", 100, "TJS", "x") is SubmitResult.Rejected)
        assertEquals(0, lookups)
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
        val after = PaymentSubmitter(store, { "MUST-NOT-BE-USED" }, { p ->
            seenKeys += p.idempotencyKey
            ApiOutcome.Ok(PostResponse("txn-9", status = "already_posted"))
        })

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
        assertNull(s.discardPending())
    }

    @Test
    fun `discard drops the payment only once the statement says it never posted`() = runBlocking {
        // Not in the statement: honoured.
        val store = storeWithPending()
        val s = submitter(store, findPosted = { ApiOutcome.Ok(false) }) { posted() }
        assertEquals(DiscardResult.Discarded, s.discardPending())
        assertNull(s.pending())

        // In the statement: the money moved — resolved as sent, not discarded.
        val postedStore = storeWithPending("key-3")
        val s2 = submitter(postedStore, findPosted = { ApiOutcome.Ok(true) }) { posted() }
        assertEquals(DiscardResult.WasPosted("key-3"), s2.discardPending())
        assertNull(postedStore.stored)

        // Statement unreachable: refuse to drop blind, keep the record.
        val unsureStore = storeWithPending()
        val s3 = submitter(unsureStore, findPosted = { ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null) }) { posted() }
        assertEquals(DiscardResult.CouldNotVerify(offline = false), s3.discardPending())
        assertNotNull(unsureStore.stored)
    }
}
