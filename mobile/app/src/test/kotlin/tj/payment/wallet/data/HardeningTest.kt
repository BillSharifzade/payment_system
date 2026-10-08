package tj.payment.wallet.data

import java.security.GeneralSecurityException
import java.util.Base64
import java.util.concurrent.CopyOnWriteArrayList
import javax.net.ssl.SSLPeerUnverifiedException
import kotlinx.coroutines.runBlocking
import okhttp3.CertificatePinner
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import okhttp3.tls.HandshakeCertificates
import okhttp3.tls.HeldCertificate
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import tj.payment.core.ApiOutcome
import tj.payment.core.Authorization
import tj.payment.core.DiscardResult
import tj.payment.core.ErrorCode
import tj.payment.core.KeyValuePendingPaymentStore
import tj.payment.core.LocalStorageException
import tj.payment.core.PaymentKind
import tj.payment.core.SecureKeyValue
import tj.payment.core.SubmitResult
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.STORAGE_MESSAGE
import tj.payment.wallet.ui.userMessage

/**
 * The hardening pass against a real OkHttp stack (MockWebServer): the
 * transaction status / void endpoints, the repository's money paths through
 * them, error-code mapping, the secure-storage boundary and certificate pinning.
 */
class HardeningTest {

    private val server = MockWebServer()
    private val requests = CopyOnWriteArrayList<RecordedRequest>()

    private class Session(var faulty: Boolean = false) : SessionStore {
        @Volatile var access: String? = "tok"
        @Volatile var refresh: String? = "r1"
        override fun accessToken(): String? {
            if (faulty) throw SecurityException("Could not decrypt value", GeneralSecurityException("keystore"))
            return access
        }
        override fun accessTokenStale(nowMs: Long) = false
        override fun setAccessToken(token: String?, expiresInSeconds: Long, nowMs: Long) {
            access = token
        }
        override fun refreshToken(): String? {
            if (faulty) throw SecurityException("Could not decrypt value")
            return refresh
        }
        override fun persistTokens(userId: String, refreshToken: String) {
            refresh = refreshToken
        }
        override fun clear() {
            access = null
            refresh = null
        }
    }

    private class MapKv : SecureKeyValue {
        val map = linkedMapOf<String, String>()
        override fun get(key: String) = map[key]
        override fun put(key: String, value: String): Boolean = true.also { map[key] = value }
        override fun remove(key: String): Boolean = true.also { map.remove(key) }
        override fun getFlag(key: String) = false
        override fun putFlag(key: String, value: Boolean) = true
    }

    private val session = Session()
    private lateinit var api: ApiClient

    @Before
    fun setUp() {
        server.start()
        api = ApiClient(server.url("/").toString().trimEnd('/'), session)
    }

    @After
    fun tearDown() {
        server.shutdown()
    }

    private fun json(body: String, status: Int = 200): MockResponse =
        MockResponse().setResponseCode(status).setHeader("Content-Type", "application/json").setBody(body)

    private fun error(status: Int, code: String) =
        json("""{"error":{"code":"$code","message":"m","request_id":"rq"}}""", status)

    private fun posted(id: String) =
        json(
            """{"transaction_id":"$id","status":"posted","created_at":"2026-10-08T06:00:00Z","kind":"transfer",""" +
                """"entries":[{"account_id":"w1","direction":"debit","amount_minor":100,"currency":"TJS"}]}""",
        )

    private fun voided(id: String) =
        json("""{"transaction_id":"$id","status":"voided","created_at":"2026-10-08T06:00:00Z"}""")

    private fun serve(handler: (RecordedRequest) -> MockResponse) {
        server.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                requests += request
                return handler(request)
            }
        }
    }

    // --- §1 / §2 request shapes ---

    @Test
    fun `transaction lookup is an authed GET by id and decodes the posted body`() = runBlocking {
        serve { posted("k1") }
        val out = api.transaction("k1")
        val status = (out as ApiOutcome.Ok).value
        assertTrue(status.isPosted)
        assertEquals(100L, status.entries.single().amountMinor)
        val req = requests.single()
        assertEquals("GET", req.method)
        assertEquals("/v1/transactions/k1", req.path)
        assertEquals("Bearer tok", req.getHeader("Authorization"))
    }

    @Test
    fun `void is an authed POST with no body and no idempotency key`() = runBlocking {
        serve { voided("k1") }
        val out = api.voidTransaction("k1")
        assertTrue((out as ApiOutcome.Ok).value.isVoided)
        val req = requests.single()
        assertEquals("POST", req.method)
        assertEquals("/v1/transactions/k1/void", req.path)
        assertEquals(0L, req.bodySize)
        assertNull(req.getHeader("Idempotency-Key"))
        assertEquals("Bearer tok", req.getHeader("Authorization"))
    }

    @Test
    fun `a coded 404 is told apart from a bare one`() = runBlocking {
        serve { error(404, "not_found") }
        val coded = api.transaction("k1") as ApiOutcome.Failed
        assertTrue(coded.codedNotFound)

        serve { MockResponse().setResponseCode(404) } // e.g. a server without the route
        val bare = api.transaction("k1") as ApiOutcome.Failed
        assertEquals(ErrorCode.NOT_FOUND, bare.code)
        assertFalse(bare.coded)
        assertFalse(bare.codedNotFound)
    }

    @Test
    fun `a 422 without an envelope is not limit_exceeded`() = runBlocking {
        serve { MockResponse().setResponseCode(422).setBody("Unprocessable Entity") }
        val out = api.wallets() as ApiOutcome.Failed
        assertEquals(ErrorCode.UNKNOWN, out.code)
        assertFalse(out.coded)

        serve { error(403, "recipient_unavailable") }
        val coded = api.wallets() as ApiOutcome.Failed
        assertEquals(ErrorCode.RECIPIENT_UNAVAILABLE, coded.code)
        assertTrue(coded.coded)
    }

    // --- the repository's money paths ---

    private var user: String? = "alice"
    private val store = KeyValuePendingPaymentStore(MapKv())

    private fun repo(approve: Boolean = true) = WalletRepository(
        api = api,
        pendingStore = store,
        currentUser = { user },
        authorizer = { if (approve) Authorization.Granted("sig") else Authorization.Cancelled },
    )

    @Test
    fun `discarding an unsettled transfer voids its key, never scans the statement`() = runBlocking {
        serve { req ->
            when {
                req.path == "/v1/transfers" -> MockResponse().setResponseCode(503)
                req.path!!.endsWith("/void") -> voided(req.path!!.split('/')[3])
                else -> error(500, "internal_error")
            }
        }
        val r = repo()
        assertEquals(SubmitResult.Unsettled(offline = false), r.submitter.submitNew("w1", "w2", 100, "TJS", "+992"))
        val key = store.load("alice")!!.idempotencyKey

        assertEquals(DiscardResult.Discarded, r.submitter.discardPending())
        assertNull(store.load("alice"))
        assertEquals(listOf("/v1/transfers", "/v1/transactions/$key/void"), requests.map { it.path })
        assertTrue("no statement scan", requests.none { it.path!!.contains("/accounts/") })
    }

    @Test
    fun `a retry refused after the first attempt posted is shown as posted, not rejected`() = runBlocking {
        var transfers = 0
        serve { req ->
            when {
                req.path == "/v1/transfers" -> if (transfers++ == 0) {
                    MockResponse().setResponseCode(504)
                } else {
                    error(422, "insufficient_funds")
                }
                req.path!!.endsWith("/void") -> posted(req.path!!.split('/')[3])
                else -> error(500, "internal_error")
            }
        }
        val r = repo()
        r.submitter.submitNew("w1", "w2", 100, "TJS", "+992")
        val key = store.load("alice")!!.idempotencyKey
        val retried = r.submitter.retryPending()
        assertEquals(SubmitResult.Posted(key, alreadyPosted = true), retried)
        assertEquals(key, requests[1].getHeader("Idempotency-Key"))
    }

    @Test
    fun `409 voided on a retry is final and nothing is resent`() = runBlocking {
        serve { req -> if (req.path == "/v1/transfers") error(409, "voided") else error(500, "internal_error") }
        val r = repo()
        val result = r.submitter.submitNew("w1", "w2", 100, "TJS", "+992")
        assertEquals(ErrorCode.VOIDED, (result as SubmitResult.Rejected).code)
        assertNull(store.load("alice"))
        assertEquals(1, requests.size)
    }

    @Test
    fun `FX survives process death with its persisted key`() = runBlocking {
        val fxKeys = mutableListOf<String?>()
        var fxCalls = 0
        serve { req ->
            if (req.path == "/v1/fx") {
                fxKeys += req.getHeader("Idempotency-Key")
                if (fxCalls++ == 0) {
                    MockResponse().setResponseCode(502)
                } else {
                    json(
                        """{"transaction_id":"${req.getHeader("Idempotency-Key")}","debited_minor":1000,"credited_minor":91,""" +
                            """"from_currency":"TJS","to_currency":"USD"}""",
                    )
                }
            } else {
                error(500, "internal_error")
            }
        }
        val first = repo().submitter.submitNew("w-tjs", "w-usd", 1_000, "TJS", "USD", kind = PaymentKind.FX)
        assertEquals(SubmitResult.Unsettled(offline = false), first)

        // A fresh repository over the same store = the app after a kill.
        val after = repo().submitter
        assertEquals(PaymentKind.FX, after.pending()?.kind)
        val result = after.retryPending() as SubmitResult.Posted
        assertEquals(91L, result.fx?.creditedMinor)
        assertEquals(2, fxKeys.size)
        assertEquals("same key across the restart", fxKeys[0], fxKeys[1])
        assertTrue(requests.all { it.path == "/v1/fx" && it.body.readUtf8().contains("\"from_account\":\"w-tjs\"") })
    }

    @Test
    fun `without device approval no request leaves the phone`() = runBlocking {
        serve { error(500, "internal_error") }
        val result = repo(approve = false).submitter.submitNew("w1", "w2", 100, "TJS", "+992")
        assertEquals(SubmitResult.NotAuthorized(null), result)
        assertEquals(0, requests.size)
        assertNull(store.load("alice"))
    }

    @Test
    fun `user B on a shared phone is not blocked by user A's unsettled payment`() = runBlocking {
        serve { req ->
            when (req.path) {
                "/v1/transfers" -> if (user == "alice") {
                    MockResponse().setResponseCode(503)
                } else {
                    json("""{"transaction_id":"${req.getHeader("Idempotency-Key")}","status":"posted"}""", 201)
                }
                else -> error(500, "internal_error")
            }
        }
        val r = repo()
        r.submitter.submitNew("a-wallet", "x", 100, "TJS", "+992 A")
        // Sign-out: AppRoot clears the cache; the record stays with alice.
        r.clearCache()
        user = "bob"
        assertNull("bob sees nothing of alice's", r.submitter.pending())
        assertTrue(r.submitter.submitNew("b-wallet", "y", 200, "TJS", "+992 B") is SubmitResult.Posted)
        user = "alice"
        assertEquals("+992 A", r.submitter.pending()?.recipientLabel)
    }

    // --- secure-storage faults are typed outcomes, never crashes (finding 6) ---

    @Test
    fun `a Keystore fault while preparing a request is Offline with a typed cause`() = runBlocking {
        serve { json("[]") }
        session.faulty = true
        val out = api.wallets()
        assertTrue("$out", out is ApiOutcome.Offline)
        out as ApiOutcome.Offline
        assertTrue(out.cause is LocalStorageException)
        assertTrue(out.localStorageFault)
        assertEquals("storage copy, not the offline copy", STORAGE_MESSAGE, out.userMessage())
        assertFalse(OFFLINE_MESSAGE == STORAGE_MESSAGE)
        assertEquals("nothing was sent", 0, requests.size)

        val restore = api.refreshSession()
        assertTrue("$restore", restore is ApiClient.RefreshOutcome.Unreachable)
    }

    // --- certificate pinning (finding 8) ---

    @Test
    fun `pinned calls succeed against the pinned key and fail closed otherwise`() = runBlocking {
        val root = HeldCertificate.Builder().certificateAuthority(0).build()
        val host = server.hostName
        val leaf = HeldCertificate.Builder().addSubjectAlternativeName(host).signedBy(root).build()
        val serverTls = HandshakeCertificates.Builder().heldCertificate(leaf, root.certificate).build()
        val clientTls = HandshakeCertificates.Builder().addTrustedCertificate(root.certificate).build()
        val tlsServer = MockWebServer().apply {
            useHttps(serverTls.sslSocketFactory(), false)
            dispatcher = object : Dispatcher() {
                override fun dispatch(request: RecordedRequest) = json("[]")
            }
            start()
        }
        try {
            val base = tlsServer.url("/").toString().trimEnd('/')
            val backup = "sha256/" + Base64.getEncoder().encodeToString(ByteArray(32) { 7 })
            val other = "sha256/" + Base64.getEncoder().encodeToString(ByteArray(32) { 9 })
            fun client(pins: List<String>) = ApiClient(
                base,
                Session(),
                certificatePins = pins,
                configureClient = { sslSocketFactory(clientTls.sslSocketFactory(), clientTls.trustManager) },
            )

            val good = client(listOf(CertificatePinner.pin(leaf.certificate), backup)).wallets()
            assertTrue("$good", good is ApiOutcome.Ok)

            val bad = client(listOf(other, backup)).wallets()
            assertTrue("$bad", bad is ApiOutcome.Offline && bad.cause is SSLPeerUnverifiedException)
        } finally {
            tlsServer.shutdown()
        }
    }

    @Test
    fun `pin configuration is validated up front`() {
        val a = "sha256/" + Base64.getEncoder().encodeToString(ByteArray(32) { 1 })
        val b = "sha256/" + Base64.getEncoder().encodeToString(ByteArray(32) { 2 })
        assertNull("no pins, no pinning (dev)", CertificatePins.pinnerFor("https://api.example.tj", emptyList()))
        assertNotNull(CertificatePins.pinnerFor("https://api.example.tj", listOf(a, b)))
        assertEquals(listOf(a, b), CertificatePins.parse(" $a ,\n$b, "))

        for (bad in listOf(
            listOf(a), // no backup pin
            listOf(a, a), // not distinct
            listOf(a, "sha1/" + b.removePrefix("sha256/")),
            listOf(a, "sha256/not-base64!!"),
            listOf(a, "sha256/" + Base64.getEncoder().encodeToString(ByteArray(20))),
        )) {
            try {
                CertificatePins.pinnerFor("https://api.example.tj", bad)
                fail("expected rejection of $bad")
            } catch (_: IllegalArgumentException) {
                // expected
            }
        }
        try {
            CertificatePins.pinnerFor("http://192.168.1.156:8099", listOf(a, b))
            fail("pins over cleartext make no sense")
        } catch (_: IllegalArgumentException) {
            // expected
        }
    }
}
