package tj.payment.wallet.data

import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import okhttp3.mockwebserver.SocketPolicy
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import tj.payment.core.ApiOutcome
import tj.payment.core.Authorization
import tj.payment.core.ErrorCode
import tj.payment.core.PayCheckRequest
import tj.payment.core.PaymentSubmitter
import tj.payment.core.PendingPayment
import tj.payment.core.PendingPaymentStore
import tj.payment.core.Settlement
import tj.payment.core.SubmitResult
import tj.payment.core.TokenResponse
import tj.payment.core.TransferRequest
import tj.payment.core.map
import tj.payment.wallet.ui.userMessage

/**
 * The token-refresh path is where a networking bug becomes a money bug (a lost
 * answer to a POST /v1/transfers) or a support ticket (a flaky refresh signing
 * the user out). These run on the JVM against MockWebServer — no device, no
 * Keystore — through the same OkHttp stack the app ships.
 */
class ApiClientTest {

    private val server = MockWebServer()
    private val session = FakeSession()
    private val refreshCalls = AtomicInteger()
    private val lastRefreshBody = AtomicReference<String?>(null)
    private lateinit var api: ApiClient

    private class FakeSession : SessionStore {
        @Volatile var access: String? = null
        @Volatile var refresh: String? = null
        @Volatile var staleAtMs: Long = Long.MAX_VALUE
        @Volatile var cleared = false

        override fun accessToken(): String? = access
        override fun accessTokenStale(nowMs: Long): Boolean = access != null && nowMs >= staleAtMs
        override fun setAccessToken(token: String?, expiresInSeconds: Long, nowMs: Long) {
            access = token
            staleAtMs = if (token != null && expiresInSeconds > 0) {
                nowMs + expiresInSeconds * 1000L * 80 / 100
            } else {
                Long.MAX_VALUE
            }
        }
        override fun refreshToken(): String? = refresh
        override fun persistTokens(userId: String, refreshToken: String) {
            refresh = refreshToken
        }
        override fun clear() {
            access = null
            refresh = null
            cleared = true
        }
    }

    private class MemoryStore : PendingPaymentStore {
        @Volatile var stored: PendingPayment? = null
        override fun load(userId: String): PendingPayment? = stored?.takeIf { it.userId == userId }
        override fun save(payment: PendingPayment): Boolean {
            stored = payment
            return true
        }
        override fun clear(userId: String, idempotencyKey: String) {
            if (stored?.idempotencyKey == idempotencyKey) stored = null
        }
    }

    /** The production wiring in miniature: transfers through this ApiClient, approval always granted. */
    private fun transferSubmitter(store: MemoryStore, key: String) = PaymentSubmitter(
        store = store,
        currentUser = { "u1" },
        newKey = { key },
        authorizer = { Authorization.Granted("sig") },
        execute = { p ->
            api.transfer(TransferRequest(p.fromAccount, p.toAccount, p.amountMinor, p.currency), p.idempotencyKey)
                .map { Settlement(it.transactionId, it.status == "already_posted") }
        },
        lookup = { api.transaction(it) },
        void = { api.voidTransaction(it) },
    )

    @Before
    fun setUp() {
        server.start()
        api = ApiClient(server.url("/").toString().trimEnd('/'), session)
    }

    @After
    fun tearDown() {
        server.shutdown()
    }

    // --- helpers ---

    private fun json(body: String): MockResponse =
        MockResponse().setHeader("Content-Type", "application/json").setBody(body)

    private fun tokens(access: String, refresh: String = "r2", expiresIn: Long = 0): MockResponse =
        json(
            """{"user_id":"u1","access_token":"$access","refresh_token":"$refresh",""" +
                """"token_type":"Bearer","expires_in":$expiresIn}""",
        )

    private fun error(status: Int, code: String, requestId: String? = "rq-1"): MockResponse =
        json(
            """{"error":{"code":"$code","message":"$code happened","request_id":""" +
                (requestId?.let { "\"$it\"" } ?: "null") + "}}",
        ).setResponseCode(status)

    /** Routes /v1/auth/refresh to [refresh] (counted) and everything else to [other]. */
    private fun serve(refresh: () -> MockResponse, other: (RecordedRequest) -> MockResponse) {
        server.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                if (request.path == "/v1/auth/refresh") {
                    refreshCalls.incrementAndGet()
                    lastRefreshBody.set(request.body.readUtf8())
                    return refresh()
                }
                return other(request)
            }
        }
    }

    private fun signedIn(access: String = "old", refresh: String = "r1") {
        session.access = access
        session.refresh = refresh
    }

    // --- (0) pay-by-QR request shape ---

    @Test
    fun `payCheck posts to the check's pay route with the key, bearer and wallet`() = runBlocking {
        signedIn()
        val seen = AtomicReference<RecordedRequest?>(null)
        serve(refresh = { tokens("new") }) { req ->
            seen.set(req)
            json("""{"transaction_id":"k1","status":"posted","check_id":"c1","amount_minor":200,"currency":"TJS"}""")
        }
        val out = api.payCheck("c1", PayCheckRequest(account = "w1"), idempotencyKey = "k1")
        assertTrue(out is ApiOutcome.Ok)
        assertEquals("k1", (out as ApiOutcome.Ok).value.transactionId)
        val request = seen.get()
        assertNotNull(request)
        assertEquals("/v1/checks/c1/pay", request!!.path)
        assertEquals("k1", request.getHeader("Idempotency-Key"))
        assertEquals("Bearer old", request.getHeader("Authorization"))
        assertTrue(request.body.readUtf8().contains(""""account":"w1""""))
    }

    // --- (a) single-flight refresh ---

    @Test
    fun `concurrent 401s trigger exactly one refresh and every call succeeds`() = runBlocking {
        signedIn()
        val walletsWithOldToken = AtomicInteger()
        serve(refresh = { tokens("new") }) { request ->
            when (request.getHeader("Authorization")) {
                "Bearer new" -> json("[]")
                else -> {
                    walletsWithOldToken.incrementAndGet()
                    error(401, "unauthorized")
                }
            }
        }

        val results = (1..8).map { async(Dispatchers.IO) { api.wallets() } }.awaitAll()

        assertTrue("all calls must recover: $results", results.all { it is ApiOutcome.Ok })
        assertEquals("exactly one POST /v1/auth/refresh", 1, refreshCalls.get())
        assertTrue("at least one call hit the 401 path", walletsWithOldToken.get() >= 1)
        assertTrue("refresh carried the persisted token", lastRefreshBody.get()!!.contains("\"refresh_token\":\"r1\""))
        assertEquals("new", session.access)
        assertEquals("rotated refresh token persisted", "r2", session.refresh)
        assertFalse(session.cleared)
    }

    // --- (b) an unreachable refresh is offline, never a sign-out ---

    @Test
    fun `refresh answering 503 keeps the session and surfaces Offline`() = runBlocking {
        signedIn()
        serve(refresh = { error(503, "retry_later") }) { error(401, "unauthorized") }

        val result = api.wallets()

        assertTrue("$result", result is ApiOutcome.Offline)
        assertEquals(1, refreshCalls.get())
        assertFalse("session must survive a 5xx refresh", session.cleared)
        assertEquals("r1", session.refresh)
        assertEquals("old", session.access)
    }

    @Test
    fun `refresh answering an unreadable 200 keeps the session and surfaces Offline`() = runBlocking {
        signedIn()
        serve(refresh = { MockResponse().setResponseCode(200).setBody("<html>captive portal</html>") }) {
            error(401, "unauthorized")
        }

        val result = api.wallets()

        assertTrue("$result", result is ApiOutcome.Offline)
        assertFalse(session.cleared)
        assertEquals("r1", session.refresh)
    }

    @Test
    fun `refresh that times out keeps the session and surfaces Offline`() = runBlocking {
        api = ApiClient(
            server.url("/").toString().trimEnd('/'),
            session,
            timeouts = ApiClient.Timeouts(readMs = 300, callMs = 3_000),
        )
        signedIn()
        serve(refresh = { MockResponse().setSocketPolicy(SocketPolicy.NO_RESPONSE) }) {
            error(401, "unauthorized")
        }

        val result = api.wallets()

        assertTrue("$result", result is ApiOutcome.Offline)
        assertEquals(1, refreshCalls.get())
        assertFalse(session.cleared)
        assertEquals("r1", session.refresh)
    }

    // --- (c) a refused refresh ends the session ---

    @Test
    fun `refresh answering 401 clears the session and the call fails unauthorized`() = runBlocking {
        signedIn()
        serve(refresh = { error(401, "unauthorized") }) { error(401, "unauthorized") }

        val result = api.wallets()

        assertTrue("$result", result is ApiOutcome.Failed)
        result as ApiOutcome.Failed
        assertEquals(ErrorCode.UNAUTHORIZED, result.code)
        assertEquals(401, result.httpStatus)
        assertTrue(session.cleared)
        assertNull(session.refresh)
        assertNull(session.access)
        assertEquals(1, refreshCalls.get())

        // Nothing left to refresh with: the next call fails fast, no second refresh.
        val again = api.wallets()
        assertTrue("$again", again is ApiOutcome.Failed && again.httpStatus == 401)
        assertEquals(1, refreshCalls.get())
    }

    @Test
    fun `refresh answering 403 also clears the session`() = runBlocking {
        signedIn()
        serve(refresh = { error(403, "forbidden") }) { error(401, "unauthorized") }

        api.wallets()

        assertTrue(session.cleared)
    }

    // --- (d) an accepted-but-unreadable transfer is unsettled, with its key intact ---

    @Test
    fun `a 201 with an HTML body leaves the transfer Unsettled and keeps the key`() = runBlocking {
        signedIn(access = "tok")
        val idempotencyHeader = AtomicReference<String?>(null)
        serve(refresh = { error(500, "internal_error") }) { request ->
            assertEquals("/v1/transfers", request.path)
            idempotencyHeader.set(request.getHeader("Idempotency-Key"))
            MockResponse()
                .setResponseCode(201)
                .setHeader("Content-Type", "text/html")
                .setBody("<html><body>Created</body></html>")
        }
        val store = MemoryStore()
        val submitter = transferSubmitter(store, "key-77")

        val result = submitter.submitNew("from", "to", 2_500, "TJS", "+992900000001")

        assertEquals(SubmitResult.Unsettled(offline = false), result)
        assertEquals("key-77", idempotencyHeader.get())
        assertEquals("the key must survive a lost answer", "key-77", store.stored?.idempotencyKey)
        assertEquals("a 2xx never trips the refresh path", 0, refreshCalls.get())
    }

    @Test
    fun `503 retry_later and 504 timeout on a transfer are Unsettled with the same key`() = runBlocking {
        signedIn(access = "tok")
        for ((status, code) in listOf(503 to "retry_later", 504 to "timeout")) {
            serve(refresh = { error(500, "internal_error") }) {
                error(status, code).setHeader("Retry-After", "2")
            }
            val store = MemoryStore()
            val submitter = transferSubmitter(store, "key-$status")

            val result = submitter.submitNew("from", "to", 100, "TJS", "x")

            assertEquals("$status $code", SubmitResult.Unsettled(offline = false), result)
            assertNotNull("$status $code must keep the key", store.stored)
        }
    }

    // --- Tagged public endpoints never enter the refresh path ---

    @Test
    fun `a 401 to login is the answer, not a refresh trigger`() = runBlocking {
        session.refresh = "r1"
        serve(refresh = { tokens("new") }) { error(401, "unauthorized") }

        val result = api.login("992900000001", "wrong-password")

        assertTrue("$result", result is ApiOutcome.Failed && result.httpStatus == 401)
        assertEquals(0, refreshCalls.get())
        assertFalse(session.cleared)
    }

    // --- Launch restore: one request, tri-state ---

    @Test
    fun `refreshSession is one request and adopts the rotated tokens`() = runBlocking {
        session.refresh = "r1"
        serve(refresh = { tokens("fresh", refresh = "r9") }) { error(500, "internal_error") }

        val outcome = api.refreshSession()

        assertTrue("$outcome", outcome is ApiClient.RefreshOutcome.Refreshed)
        assertEquals(1, server.requestCount)
        assertEquals("fresh", session.access)
        assertEquals("r9", session.refresh)
    }

    @Test
    fun `refreshSession reports Unreachable on a 5xx and Dead on a 401`() = runBlocking {
        session.refresh = "r1"
        serve(refresh = { error(502, "unknown") }) { error(500, "internal_error") }
        assertTrue(api.refreshSession() is ApiClient.RefreshOutcome.Unreachable)
        assertFalse(session.cleared)

        serve(refresh = { error(401, "unauthorized") }) { error(500, "internal_error") }
        assertEquals(ApiClient.RefreshOutcome.Dead, api.refreshSession())
        assertTrue(session.cleared)
    }

    // --- Error envelope: request_id reaches the user copy for 5xx ---

    @Test
    fun `request_id is parsed and quoted for server-side failures`() = runBlocking {
        signedIn(access = "tok")
        serve(refresh = { error(500, "internal_error") }) { error(500, "internal_error", requestId = "rq-42") }

        val result = api.wallets() as ApiOutcome.Failed

        assertEquals(ErrorCode.INTERNAL_ERROR, result.code)
        assertEquals("rq-42", result.requestId)
        assertTrue(result.userMessage(), result.userMessage().endsWith("Ref: rq-42"))

        serve(refresh = { error(500, "internal_error") }) { error(422, "insufficient_funds", requestId = "rq-43") }
        val refusal = api.wallets() as ApiOutcome.Failed
        assertFalse("no Ref for a 4xx", refusal.userMessage().contains("Ref:"))
    }

    // --- Proactive rotation at ~80% of expires_in ---

    @OptIn(ExperimentalCoroutinesApi::class) // advanceTimeBy / runCurrent
    @Test
    fun `a proactive refresh fires at 80 percent of expires_in while on screen`() {
        val testScope = TestScope()
        api = ApiClient(
            server.url("/").toString().trimEnd('/'),
            session,
            foreground = MutableStateFlow(true),
            scope = testScope,
            clock = { 0L },
        )
        // The rotated token has no lifetime, so no further rotation is queued.
        serve(refresh = { tokens("rotated", expiresIn = 0) }) { json("[]") }

        api.adoptTokens(TokenResponse("u1", "first", "r1", expiresInSeconds = 100))

        testScope.runTest {
            advanceTimeBy(79_999)
            runCurrent()
            assertEquals("not before 80%", 0, refreshCalls.get())

            advanceTimeBy(2)
            runCurrent()
            // The job is now doing real I/O on Dispatchers.IO: wait for it in real time.
            withContext(Dispatchers.Default) {
                withTimeout(5_000) {
                    while (session.access != "rotated") delay(20)
                }
            }
        }

        assertEquals(1, refreshCalls.get())
        assertEquals("rotated", session.access)
        assertEquals("r2", session.refresh)
    }
}
