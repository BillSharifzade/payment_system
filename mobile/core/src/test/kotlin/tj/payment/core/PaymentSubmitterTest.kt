package tj.payment.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The idempotency state machine is where a client-side money bug would hide:
 * a lost key can double-charge, a dropped key can lose a payment's outcome, a
 * shared record can lock a second user out. Every transition is pinned here,
 * on the JVM, with fakes.
 */
class PaymentSubmitterTest {

    /** Per-user slots, like the real store. */
    class FakeStore(var persists: Boolean = true) : PendingPaymentStore {
        val records = linkedMapOf<String, PendingPayment>()
        var faulty = false
        override fun load(userId: String): PendingPayment? {
            if (faulty) throw LocalStorageException("keystore down")
            return records[userId]
        }
        override fun save(payment: PendingPayment): Boolean {
            if (!persists) return false
            records[payment.userId] = payment
            return true
        }
        override fun clear(userId: String, idempotencyKey: String) {
            if (records[userId]?.idempotencyKey == idempotencyKey) records.remove(userId)
        }
        fun of(user: String = "alice") = records[user]
    }

    private class Net {
        val executed = mutableListOf<PendingPayment>()
        val lookups = mutableListOf<String>()
        val voids = mutableListOf<String>()
        var execute: (PendingPayment) -> ApiOutcome<Settlement> = { posted() }
        var lookup: (String) -> ApiOutcome<TransactionStatusDto> = { notFound() }
        var void: (String) -> ApiOutcome<TransactionStatusDto> = { id -> ApiOutcome.Ok(voided(id)) }
    }

    private class Auth {
        val asked = mutableListOf<PendingPayment>()
        var answer: (PendingPayment) -> Authorization = { p -> Authorization.Granted("sig:" + p.idempotencyKey) }
        val authorizer = PaymentAuthorizer { p -> asked += p; answer(p) }
    }

    private var user: String? = "alice"

    private fun submitter(
        store: FakeStore,
        net: Net = Net(),
        auth: Auth = Auth(),
        keys: MutableList<String> = mutableListOf("key-1", "key-2", "key-3"),
    ) = PaymentSubmitter(
        store = store,
        currentUser = { user },
        newKey = { keys.removeAt(0) },
        authorizer = auth.authorizer,
        execute = { p -> net.executed += p; net.execute(p) },
        lookup = { id -> net.lookups += id; net.lookup(id) },
        void = { id -> net.voids += id; net.void(id) },
        clock = { 1_000L },
        io = Dispatchers.Unconfined,
    )

    /** A store holding an unsettled payment from "a previous run". */
    private fun storeWithPending(key: String = "key-1", owner: String = "alice") = FakeStore().apply {
        records[owner] = PendingPayment(key, "from", "to", 100, "TJS", "x", userId = owner, authorization = "sig")
    }

    private suspend fun PaymentSubmitter.send(amount: Long = 100, to: String = "to") =
        submitNew("from", to, amount, "TJS", "992900000001")

    // --- the happy path and the persist-before-network invariant ---

    @Test
    fun `success posts once, clears the record, and the record carried owner and approval`() = runBlocking {
        val store = FakeStore()
        val net = Net()
        val result = submitter(store, net).send(2_500)

        assertEquals(SubmitResult.Posted("txn-1", alreadyPosted = false), result)
        assertNull(store.of())
        assertEquals(1, net.executed.size)
        val sent = net.executed.single()
        assertEquals("key-1", sent.idempotencyKey)
        assertEquals("alice", sent.userId)
        assertEquals("sig:key-1", sent.authorization)
        assertEquals(PaymentKind.TRANSFER, sent.kind)
        assertEquals(1_000L, sent.createdAtMs)
    }

    @Test
    fun `payment is durable, with its approval, before the first network attempt`() = runBlocking {
        val store = FakeStore()
        val net = Net()
        var storedAtAttemptTime: PendingPayment? = null
        net.execute = { storedAtAttemptTime = store.of(); posted() }

        submitter(store, net).send()

        assertEquals("key-1", storedAtAttemptTime?.idempotencyKey)
        assertEquals("sig:key-1", storedAtAttemptTime?.authorization)
    }

    @Test
    fun `a persist failure never reaches the network`() = runBlocking {
        val store = FakeStore(persists = false)
        val net = Net()
        val s = submitter(store, net)

        assertEquals(SubmitResult.NotStarted, s.send())
        assertEquals("no request may go out without a durable key", 0, net.executed.size)
        store.persists = true
        assertTrue(s.send() is SubmitResult.Posted)
    }

    // --- authorization (finding 7) ---

    @Test
    fun `no approval, no record, no request`() = runBlocking {
        for ((answer, expected) in listOf(
            Authorization.Cancelled to SubmitResult.NotAuthorized(null),
            Authorization.Denied(AuthDenial.NOT_ENROLLED) to SubmitResult.NotAuthorized(AuthDenial.NOT_ENROLLED),
            Authorization.Denied(AuthDenial.NO_SCREEN) to SubmitResult.NotAuthorized(AuthDenial.NO_SCREEN),
        )) {
            val store = FakeStore()
            val net = Net()
            val auth = Auth().apply { this.answer = { answer } }
            assertEquals(expected, submitter(store, net, auth).send())
            assertNull("nothing stored for $answer", store.of())
            assertEquals("nothing sent for $answer", 0, net.executed.size)
        }
    }

    @Test
    fun `the approval covers exactly the payment that is then sent`() = runBlocking {
        val store = FakeStore()
        val net = Net()
        val auth = Auth()
        submitter(store, net, auth).submitNew("w-from", "w-to", 4_200, "TJS", "Firuz", checkId = null)

        val approved = auth.asked.single()
        val sent = net.executed.single()
        assertEquals(approved.copy(authorization = sent.authorization), sent)
        assertTrue(approved.authorizationPayload().contentEquals(sent.authorizationPayload()))
    }

    @Test
    fun `a retry reuses the original approval and never prompts again`() = runBlocking {
        val store = FakeStore()
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException("down")) } }
        val auth = Auth()
        val s = submitter(store, net, auth)
        s.send()
        net.execute = { posted() }
        assertTrue(s.retryPending() is SubmitResult.Posted)
        assertEquals("one prompt for one intent", 1, auth.asked.size)
        assertEquals(listOf("sig:key-1", "sig:key-1"), net.executed.map { it.authorization })
    }

    @Test
    fun `the device that signed is stored with the signature and resent on every attempt`() = runBlocking {
        val store = FakeStore()
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException("down")) } }
        val auth = Auth().apply { answer = { p -> Authorization.Granted("sig:" + p.idempotencyKey, deviceId = "dev-1") } }
        val s = submitter(store, net, auth)
        s.send()
        assertEquals("dev-1", store.of()?.deviceId)
        net.execute = { posted() }
        assertTrue(s.retryPending() is SubmitResult.Posted)
        val expected = mapOf("X-Device-Id" to "dev-1", "X-Device-Signature" to "sig:key-1")
        assertEquals(listOf(expected, expected), net.executed.map { it.deviceHeaders() })
    }

    @Test
    fun `an unregistered device is a refusal before anything is stored or sent`() = runBlocking {
        val store = FakeStore()
        val net = Net()
        val auth = Auth().apply { answer = { Authorization.Denied(AuthDenial.DEVICE_NOT_REGISTERED) } }
        assertEquals(SubmitResult.NotAuthorized(AuthDenial.DEVICE_NOT_REGISTERED), submitter(store, net, auth).send())
        assertNull(store.of())
        assertEquals(0, net.executed.size)
    }

    @Test
    fun `a device-signature refusal of a first attempt is definitive`() = runBlocking {
        for (code in listOf(ErrorCode.DEVICE_SIGNATURE_INVALID, ErrorCode.DEVICE_SIGNATURE_REQUIRED)) {
            val store = FakeStore()
            val net = Net().apply { execute = { ApiOutcome.Failed(code, 403, "device", coded = true) } }
            assertEquals(SubmitResult.Rejected(code, "device"), submitter(store, net).send())
            assertNull("the server ruled before posting; nothing to keep", store.of())
            assertEquals(0, net.voids.size)
        }
    }

    @Test
    fun `signed out, nothing starts and nobody is prompted`() = runBlocking {
        user = null
        val store = FakeStore()
        val net = Net()
        val auth = Auth()
        assertEquals(SubmitResult.NotStarted, submitter(store, net, auth).send())
        assertEquals(0, auth.asked.size)
        assertEquals(0, net.executed.size)
    }

    // --- unknown outcomes keep the key ---

    @Test
    fun `offline keeps the payment and retry reuses the SAME key`() = runBlocking {
        val store = FakeStore()
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException("no network")) } }
        val s = submitter(store, net)

        assertEquals(SubmitResult.Unsettled(offline = true), s.send())
        assertNotNull(store.of())

        net.execute = { posted() }
        assertTrue(s.retryPending() is SubmitResult.Posted)
        assertNull(store.of())
        assertEquals(listOf("key-1", "key-1"), net.executed.map { it.idempotencyKey })
    }

    @Test
    fun `rate limited, 5xx, retry_later and timeout keep the key, a first definitive 4xx clears it`() = runBlocking {
        for ((outcome, kept) in listOf(
            ApiOutcome.Failed(ErrorCode.RATE_LIMITED, 429, null) to true,
            ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null) to true,
            ApiOutcome.Failed(ErrorCode.RETRY_LATER, 503, null) to true,
            ApiOutcome.Failed(ErrorCode.TIMEOUT, 504, null) to true,
            ApiOutcome.Failed(ErrorCode.UNKNOWN, 502, null) to true,
            ApiOutcome.Failed(ErrorCode.INSUFFICIENT_FUNDS, 422, null) to false,
            ApiOutcome.Failed(ErrorCode.REJECTED, 422, null) to false,
            ApiOutcome.Failed(ErrorCode.RECIPIENT_UNAVAILABLE, 403, null) to false,
            ApiOutcome.Failed(ErrorCode.BAD_REQUEST, 400, null) to false,
            ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, null) to false,
            ApiOutcome.Failed(ErrorCode.KYC_REQUIRED, 403, null) to false,
        )) {
            val store = FakeStore()
            val net = Net().apply { execute = { outcome } }
            val result = submitter(store, net).send()
            if (kept) {
                assertEquals("$outcome", SubmitResult.Unsettled(offline = false), result)
                assertNotNull("expected key kept for $outcome", store.of())
            } else {
                assertEquals(SubmitResult.Rejected(outcome.code, null), result)
                assertNull("expected key cleared for $outcome", store.of())
                assertTrue("a first answer needs no void", net.voids.isEmpty())
            }
        }
    }

    @Test
    fun `a 2xx whose body could not be read is Unsettled, never Rejected`() = runBlocking {
        for (status in listOf(200, 201, 204)) {
            val store = FakeStore()
            val net = Net().apply { execute = { ApiOutcome.Failed(ErrorCode.UNKNOWN, status, "malformed response") } }
            assertEquals("status $status", SubmitResult.Unsettled(offline = false), submitter(store, net).send())
            assertNotNull("key must survive a lost $status answer", store.of())
        }
    }

    // --- retries: refusals are confirmed by voiding the key (finding 2) ---

    @Test
    fun `a retry refused by an expired session stays Unsettled without a void`() = runBlocking {
        val store = storeWithPending()
        val net = Net().apply { execute = { ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, null, coded = true) } }
        assertEquals(SubmitResult.Unsettled(offline = false), submitter(store, net).retryPending())
        assertNotNull(store.of())
        assertTrue(net.voids.isEmpty())
    }

    @Test
    fun `a definitive refusal of a retry is confirmed by voiding the key`() = runBlocking {
        val refusal = ApiOutcome.Failed(ErrorCode.INSUFFICIENT_FUNDS, 422, "no funds", coded = true)

        // Void answers "voided": the key can never post, the refusal stands.
        val voidedStore = storeWithPending("key-7")
        val net1 = Net().apply { execute = { refusal } }
        assertEquals(SubmitResult.Rejected(ErrorCode.INSUFFICIENT_FUNDS, "no funds"), submitter(voidedStore, net1).retryPending())
        assertNull(voidedStore.of())
        assertEquals(listOf("key-7"), net1.voids)

        // Void answers "posted": the FIRST attempt went through after all.
        val postedStore = storeWithPending("key-8")
        val net2 = Net().apply {
            execute = { refusal }
            void = { id -> ApiOutcome.Ok(postedStatus(id)) }
        }
        assertEquals(SubmitResult.Posted("key-8", alreadyPosted = true), submitter(postedStore, net2).retryPending())
        assertNull(postedStore.of())

        // Void unreachable: never forget a key on a hunch.
        for (unsure in listOf<ApiOutcome<TransactionStatusDto>>(
            ApiOutcome.Offline(RuntimeException("down")),
            ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null, coded = true),
            // An UNCODED 404 (a proxy, an older server without the route) proves nothing.
            ApiOutcome.Failed(ErrorCode.NOT_FOUND, 404, null, coded = false),
        )) {
            val unsureStore = storeWithPending()
            val net3 = Net().apply {
                execute = { refusal }
                void = { unsure }
            }
            val result = submitter(unsureStore, net3).retryPending()
            assertTrue("$unsure -> $result", result is SubmitResult.Unsettled)
            assertNotNull("$unsure must keep the key", unsureStore.of())
        }

        // A CODED 404: the key is a transaction none of this user's accounts is in.
        val foreignStore = storeWithPending()
        val net4 = Net().apply {
            execute = { refusal }
            void = { ApiOutcome.Failed(ErrorCode.NOT_FOUND, 404, "not found", coded = true) }
        }
        assertEquals(SubmitResult.Rejected(ErrorCode.INSUFFICIENT_FUNDS, "no funds"), submitter(foreignStore, net4).retryPending())
        assertNull(foreignStore.of())
    }

    @Test
    fun `409 voided on a submit or retry is definitive and needs no second request`() = runBlocking {
        val voided = ApiOutcome.Failed(ErrorCode.VOIDED, 409, "voided", coded = true)
        val store = storeWithPending()
        val net = Net().apply { execute = { voided } }
        assertEquals(SubmitResult.Rejected(ErrorCode.VOIDED, "voided"), submitter(store, net).retryPending())
        assertNull(store.of())
        assertTrue(net.voids.isEmpty())
        assertTrue(net.lookups.isEmpty())
    }

    @Test
    fun `duplicate_transaction means look it up, never rejected`() = runBlocking {
        val duplicate = ApiOutcome.Failed(ErrorCode.DUPLICATE_TRANSACTION, 409, "dup", coded = true)

        // Retention expired, the payment had posted: shown as sent.
        val store = storeWithPending("key-5")
        val net = Net().apply {
            execute = { duplicate }
            lookup = { id -> ApiOutcome.Ok(postedStatus(id)) }
        }
        assertEquals(SubmitResult.Posted("key-5", alreadyPosted = true), submitter(store, net).retryPending())
        assertEquals(listOf("key-5"), net.lookups)
        assertNull(store.of())

        // Lookup inconclusive (404 / offline): kept, never "rejected".
        for (answer in listOf<ApiOutcome<TransactionStatusDto>>(notFound(), ApiOutcome.Offline(RuntimeException()))) {
            val keep = storeWithPending()
            val n = Net().apply {
                execute = { duplicate }
                lookup = { answer }
            }
            assertTrue(submitter(keep, n).retryPending() is SubmitResult.Unsettled)
            assertNotNull(keep.of())
        }

        // Same on a first attempt (cannot happen with a fresh UUID — but never "rejected").
        val first = FakeStore()
        val n2 = Net().apply {
            execute = { duplicate }
            lookup = { id -> ApiOutcome.Ok(postedStatus(id)) }
        }
        assertEquals(SubmitResult.Posted("key-1", alreadyPosted = true), submitter(first, n2).send())
    }

    // --- per-user binding (finding 1) ---

    @Test
    fun `a second submit is blocked for the same user while one is unsettled`() = runBlocking {
        val store = FakeStore()
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException("down")) } }
        val s = submitter(store, net)

        s.send(100)
        val second = s.send(200, to = "to2")
        assertTrue(second is SubmitResult.Blocked)
        assertEquals(100L, (second as SubmitResult.Blocked).pending.amountMinor)
        assertEquals(100L, store.of()?.amountMinor)
    }

    @Test
    fun `another user on the same phone never sees, resolves or is blocked by it`() = runBlocking {
        val store = FakeStore()
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException("down")) } }
        val s = submitter(store, net)
        s.send(100) // alice's payment is now unsettled

        user = "bob"
        assertNull("bob must not see alice's payment", s.pending())
        assertNull("bob cannot retry it", s.retryPending())
        assertNull("bob cannot discard it", s.discardPending())
        assertNull("bob cannot check it", s.checkPending())
        net.execute = { posted("txn-bob") }
        assertEquals("bob pays normally", SubmitResult.Posted("txn-bob", alreadyPosted = false), s.send(700, to = "carol"))
        assertNotNull("alice's record is untouched", store.of("alice"))
        assertNull("bob's settled payment left nothing behind", store.of("bob"))
        assertEquals(listOf("key-1", "key-2"), net.executed.map { it.idempotencyKey })
        assertEquals(listOf("alice", "bob"), net.executed.map { it.userId })

        user = "alice"
        assertEquals("alice sees her payment again", "key-1", s.pending()?.idempotencyKey)
    }

    @Test
    fun `app restart resumes the stored payment with its original key`() = runBlocking {
        val store = FakeStore()
        val before = submitter(store, Net().apply { execute = { ApiOutcome.Offline(RuntimeException("down")) } })
        before.submitNew("from", "to", 300, "TJS", "992901234567")

        val net = Net().apply { execute = { posted("txn-9", alreadyPosted = true) } }
        val after = submitter(store, net, keys = mutableListOf()) // must not mint a key

        assertEquals("992901234567", after.pending()?.recipientLabel)
        assertEquals(SubmitResult.Posted("txn-9", alreadyPosted = true), after.retryPending())
        assertEquals(listOf("key-1"), net.executed.map { it.idempotencyKey })
        assertNull(store.of())
    }

    @Test
    fun `nothing pending makes every resolution a no-op`() = runBlocking {
        val s = submitter(FakeStore())
        assertNull(s.retryPending())
        assertNull(s.discardPending())
        assertNull(s.checkPending())
        assertNull(s.pending())
    }

    // --- discard voids first ---

    @Test
    fun `discard voids the key and drops the record only on a definitive answer`() = runBlocking {
        val cases: List<Pair<ApiOutcome<TransactionStatusDto>, DiscardResult>> = listOf(
            ApiOutcome.Ok(voided("key-1")) to DiscardResult.Discarded,
            ApiOutcome.Ok(postedStatus("key-1")) to DiscardResult.WasPosted("key-1"),
            ApiOutcome.Failed(ErrorCode.NOT_FOUND, 404, "nf", coded = true) to DiscardResult.Discarded,
            ApiOutcome.Failed(ErrorCode.NOT_FOUND, 404, null, coded = false) to DiscardResult.CouldNotVerify(false),
            ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null, coded = true) to DiscardResult.CouldNotVerify(false),
            ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, null, coded = true) to DiscardResult.CouldNotVerify(false),
            ApiOutcome.Offline(RuntimeException("down")) to DiscardResult.CouldNotVerify(true),
        )
        for ((answer, expected) in cases) {
            val store = storeWithPending()
            val net = Net().apply { void = { answer } }
            assertEquals("$answer", expected, submitter(store, net).discardPending())
            val dropped = expected !is DiscardResult.CouldNotVerify
            assertEquals("record dropped for $answer?", dropped, store.of() == null)
            assertEquals(listOf("key-1"), net.voids)
            assertTrue("discard never scans or looks up", net.lookups.isEmpty())
        }
    }

    // --- read-only status check (Home / startup) ---

    @Test
    fun `checkPending resolves posted and voided keys and keeps the rest`() = runBlocking {
        val cases: List<Pair<ApiOutcome<TransactionStatusDto>, Boolean>> = listOf(
            ApiOutcome.Ok(postedStatus("key-1")) to true,
            ApiOutcome.Ok(voided("key-1")) to true,
            notFound() to false,
            ApiOutcome.Offline(RuntimeException()) to false,
            ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 500, null) to false,
        )
        for ((answer, cleared) in cases) {
            val store = storeWithPending()
            val net = Net().apply { lookup = { answer } }
            val checked = submitter(store, net).checkPending()
            assertNotNull(checked)
            assertEquals("$answer", cleared, store.of() == null)
            assertTrue("a check never moves money", net.executed.isEmpty() && net.voids.isEmpty())
            when (answer) {
                is ApiOutcome.Ok -> assertTrue(checked is PendingCheck.Posted || checked is PendingCheck.Voided)
                is ApiOutcome.Failed -> assertTrue(
                    "$answer -> $checked",
                    if (answer.codedNotFound) checked is PendingCheck.NotPosted else checked is PendingCheck.Unknown,
                )
                is ApiOutcome.Offline -> assertEquals(PendingCheck.Unknown(store.of()!!, offline = true), checked)
            }
        }
    }

    // --- storage faults (finding 6) ---

    @Test
    fun `an unreadable store never starts a payment and never reads as nothing pending`() = runBlocking {
        val store = storeWithPending().apply { faulty = true }
        val net = Net()
        val auth = Auth()
        val s = submitter(store, net, auth)

        assertEquals(SubmitResult.NotStarted, s.send())
        assertEquals(0, auth.asked.size)
        assertEquals(0, net.executed.size)
        assertEquals(SubmitResult.NotStarted, s.retryPending())
        assertEquals(DiscardResult.CouldNotVerify(offline = false), s.discardPending())
        assertTrue(net.voids.isEmpty())
    }

    // --- FX rides the same machine (finding 3) ---

    @Test
    fun `an FX conversion is persisted, approved and retried like a transfer`() = runBlocking {
        val store = FakeStore()
        val fx = FxResponse("key-1", debitedMinor = 1_000, creditedMinor = 91, fromCurrency = "TJS", toCurrency = "USD")
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException("lost")) } }
        val s = submitter(store, net)

        val first = s.submitNew("w-tjs", "w-usd", 1_000, "TJS", "USD", kind = PaymentKind.FX)
        assertEquals(SubmitResult.Unsettled(offline = true), first)
        assertEquals(PaymentKind.FX, store.of()?.kind)

        // "Process death": a new submitter over the same store, same key.
        net.execute = { p -> ApiOutcome.Ok(Settlement(p.idempotencyKey, alreadyPosted = false, fx = fx)) }
        val after = submitter(store, net, keys = mutableListOf())
        assertEquals(SubmitResult.Posted("key-1", alreadyPosted = false, fx = fx), after.retryPending())
        assertEquals(listOf("key-1", "key-1"), net.executed.map { it.idempotencyKey })
        assertTrue(net.executed.all { it.kind == PaymentKind.FX })
    }

    @Test
    fun `pendingFlow follows the current user's record`() = runBlocking {
        val store = FakeStore()
        val net = Net().apply { execute = { ApiOutcome.Offline(RuntimeException()) } }
        val s = submitter(store, net)
        assertNull(s.pendingFlow.value)
        s.send()
        assertEquals("key-1", s.pendingFlow.value?.idempotencyKey)
        net.execute = { posted() }
        s.retryPending()
        assertNull(s.pendingFlow.value)
        s.send() // key-2 posts at once
        assertNull(s.pendingFlow.value)
        s.forget()
        assertFalse(s.pendingFlow.value != null)
    }

    companion object {
        fun posted(id: String = "txn-1", alreadyPosted: Boolean = false): ApiOutcome<Settlement> =
            ApiOutcome.Ok(Settlement(id, alreadyPosted))

        fun voided(id: String) = TransactionStatusDto(id, TransactionStatusDto.STATUS_VOIDED)

        fun postedStatus(id: String) = TransactionStatusDto(id, TransactionStatusDto.STATUS_POSTED, kind = "transfer")

        fun notFound(): ApiOutcome<TransactionStatusDto> =
            ApiOutcome.Failed(ErrorCode.NOT_FOUND, 404, "not found", coded = true)
    }
}
