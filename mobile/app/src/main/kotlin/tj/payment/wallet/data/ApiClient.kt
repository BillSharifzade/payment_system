package tj.payment.wallet.data

import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.KSerializer
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.builtins.serializer
import kotlinx.serialization.json.Json
import okhttp3.Authenticator
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import okhttp3.Route
import tj.payment.core.AccountResponse
import tj.payment.core.ApiOutcome
import tj.payment.core.CheckDto
import tj.payment.core.CreateCheckRequest
import tj.payment.core.PayCheckRequest
import tj.payment.core.ClientConfigResponse
import tj.payment.core.CreateWalletRequest
import tj.payment.core.CredentialsRequest
import tj.payment.core.DocumentResponse
import tj.payment.core.ErrorCode
import tj.payment.core.ErrorEnvelope
import tj.payment.core.FxRateDto
import tj.payment.core.FxRequest
import tj.payment.core.FxResponse
import tj.payment.core.KycStatusResponse
import tj.payment.core.KycSubmissionDto
import tj.payment.core.PostResponse
import tj.payment.core.RefreshRequest
import tj.payment.core.ResolveResponse
import tj.payment.core.StatementResponse
import tj.payment.core.SubmitKycRequest
import tj.payment.core.TokenResponse
import tj.payment.core.TransferRequest
import tj.payment.core.WalletDto

/**
 * The single HTTP boundary to the payment backend. Thin on purpose: OkHttp +
 * kotlinx-serialization, no Retrofit proxies or reflection — fast, small, and
 * every request path is explicit and auditable.
 *
 * Cross-cutting behaviours live here so no call site can forget them:
 *  - an interceptor attaches the Bearer access token, rotating it first when it
 *    is near expiry (so the first request after idle never pays a 401 round trip);
 *  - an [Authenticator] refreshes a rotated/expired token on a 401 — single-flight
 *    — and retries the original request once;
 *  - a refresh that cannot be completed (network, 5xx, unreadable body) is
 *    reported as *offline*, never as *signed out*: only the server saying 401/403
 *    to the refresh token ends a session. The backend keeps a short reuse-grace
 *    window for a just-rotated refresh token, so a lost refresh answer recovers;
 *  - a proactive rotation is scheduled at ~80% of `expires_in` while the app is
 *    on screen.
 *
 * No Android types here: this class unit-tests on the JVM against MockWebServer.
 */
class ApiClient(
    private val baseUrl: String,
    private val session: SessionStore,
    timeouts: Timeouts = Timeouts(),
    /** Whether the app is on screen; the proactive-refresh timer waits for it. */
    private val foreground: StateFlow<Boolean> = MutableStateFlow(true),
    private val scope: CoroutineScope = CoroutineScope(SupervisorJob() + Dispatchers.Default),
    private val clock: () -> Long = System::currentTimeMillis,
) {
    data class Timeouts(
        val connectMs: Long = 10_000,
        val readMs: Long = 20_000,
        val writeMs: Long = 20_000,
        /** Ceiling on a whole call incl. the authenticator's refresh + retry. */
        val callMs: Long = 45_000,
    )

    /** Outcome of exchanging the refresh token. Tri-state on purpose. */
    sealed interface RefreshOutcome {
        /** A usable access token is in the session (rotated by us, or by a concurrent caller). */
        data class Refreshed(val accessToken: String) : RefreshOutcome

        /** The server said 401/403 to the refresh token: the family is revoked or expired. Session cleared. */
        data object Dead : RefreshOutcome

        /** Transport failure, 5xx, or an unreadable body: unknown; the session is kept. */
        data class Unreachable(val cause: IOException) : RefreshOutcome
    }

    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }
    private val jsonMedia = "application/json; charset=utf-8".toMediaType()

    private val client: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(timeouts.connectMs, TimeUnit.MILLISECONDS)
        .readTimeout(timeouts.readMs, TimeUnit.MILLISECONDS)
        .writeTimeout(timeouts.writeMs, TimeUnit.MILLISECONDS)
        .callTimeout(timeouts.callMs, TimeUnit.MILLISECONDS)
        // A redirect would re-send the bearer (and a money-moving POST body)
        // wherever it pointed. The API never redirects; treat one as an error.
        .followRedirects(false)
        .followSslRedirects(false)
        // TODO(prod TLS): .certificatePinner(...) goes here — the bare client
        // below derives from this one, so pinning applies to the refresh call too.
        .addInterceptor(AuthHeaderInterceptor())
        .authenticator(RefreshAuthenticator())
        .build()

    /**
     * For the refresh call itself: the main client minus its auth plumbing (no
     * recursion), sharing the connection pool, dispatcher, timeouts and any
     * future pinner instead of standing up a second stack per refresh.
     */
    private val bareClient: OkHttpClient by lazy {
        client.newBuilder()
            .authenticator(Authenticator.NONE)
            .apply {
                interceptors().clear()
                networkInterceptors().clear()
            }
            .build()
    }

    // --- Public endpoints (no bearer; a 401 here is the answer, never a refresh trigger) ---

    suspend fun register(phone: String, password: String): ApiOutcome<TokenResponse> =
        call(
            post("/v1/auth/register", CredentialsRequest.serializer(), CredentialsRequest(phone, password), authed = false),
            TokenResponse.serializer(),
        )

    suspend fun login(phone: String, password: String): ApiOutcome<TokenResponse> =
        call(
            post("/v1/auth/login", CredentialsRequest.serializer(), CredentialsRequest(phone, password), authed = false),
            TokenResponse.serializer(),
        )

    suspend fun logout(refreshToken: String): ApiOutcome<Unit> =
        callIgnoringBody(post("/v1/auth/logout", RefreshRequest.serializer(), RefreshRequest(refreshToken), authed = false))

    /**
     * Re-establish a session from the persisted refresh token in ONE request
     * (launch restore). Adopts the new tokens; clears the session on [RefreshOutcome.Dead].
     */
    suspend fun refreshSession(): RefreshOutcome = withContext(Dispatchers.IO) {
        ensureFreshToken(staleToken = session.accessToken())
    }

    /**
     * Make [tokens] the session: refresh token persisted first (if we die right
     * here, the next launch still holds the valid, newest one), then the access
     * token in memory with its lifetime, then a proactive rotation scheduled.
     */
    fun adoptTokens(tokens: TokenResponse) {
        session.persistTokens(tokens.userId, tokens.refreshToken)
        session.setAccessToken(tokens.accessToken, tokens.expiresInSeconds, clock())
        scheduleProactiveRefresh(tokens.accessToken, tokens.expiresInSeconds)
    }

    // --- Authenticated endpoints ---

    suspend fun wallets(): ApiOutcome<List<WalletDto>> =
        call(get("/v1/wallets"), ListSerializer(WalletDto.serializer()))

    suspend fun createWallet(currency: String): ApiOutcome<AccountResponse> =
        call(post("/v1/wallets", CreateWalletRequest.serializer(), CreateWalletRequest(currency)), AccountResponse.serializer())

    suspend fun config(): ApiOutcome<ClientConfigResponse> =
        call(get("/v1/config"), ClientConfigResponse.serializer())

    /** The "check number" / QR-scan lookup used by the send flow. */
    suspend fun resolveByPhone(phone: String): ApiOutcome<ResolveResponse> =
        call(get("/v1/users/resolve?phone=${phone.urlEncode()}"), ResolveResponse.serializer())

    suspend fun resolveByWallet(walletId: String): ApiOutcome<ResolveResponse> =
        call(get("/v1/users/resolve?wallet=${walletId.urlEncode()}"), ResolveResponse.serializer())

    /** One page of an account's statement, newest first. */
    suspend fun statement(accountId: String, cursor: String?, limit: Int): ApiOutcome<StatementResponse> {
        val query = buildString {
            append("?limit=").append(limit)
            if (cursor != null) append("&cursor=").append(cursor.urlEncode())
        }
        return call(get("/v1/accounts/${accountId.urlEncode()}/transactions$query"), StatementResponse.serializer())
    }

    /** Post a transfer. [idempotencyKey] comes from the PaymentSubmitter — the
     * same key MUST be resent on retries so the server charges at most once. */
    suspend fun transfer(request: TransferRequest, idempotencyKey: String): ApiOutcome<PostResponse> =
        call(post("/v1/transfers", TransferRequest.serializer(), request, idempotencyKey = idempotencyKey), PostResponse.serializer())

    suspend fun kycStatus(): ApiOutcome<KycStatusResponse> =
        call(get("/v1/kyc"), KycStatusResponse.serializer())

    /** Upload an identity document (multipart field `file`). */
    suspend fun uploadKycDocument(bytes: ByteArray, mimeType: String): ApiOutcome<DocumentResponse> {
        val body = MultipartBody.Builder()
            .setType(MultipartBody.FORM)
            .addFormDataPart("file", "document", bytes.toRequestBody(mimeType.toMediaType()))
            .build()
        val request = Request.Builder().url("$baseUrl/v1/kyc/documents").post(body).build()
        return call(request, DocumentResponse.serializer())
    }

    suspend fun submitKyc(request: SubmitKycRequest): ApiOutcome<KycSubmissionDto> =
        call(post("/v1/kyc/submissions", SubmitKycRequest.serializer(), request), KycSubmissionDto.serializer())

    suspend fun fxRates(): ApiOutcome<List<FxRateDto>> =
        call(get("/v1/fx/rates"), ListSerializer(FxRateDto.serializer()))

    /** Convert between the caller's own wallets. Same idempotency contract as [transfer]. */
    suspend fun fx(request: FxRequest, idempotencyKey: String): ApiOutcome<FxResponse> =
        call(post("/v1/fx", FxRequest.serializer(), request, idempotencyKey = idempotencyKey), FxResponse.serializer())

    // --- Checks: request money by QR, pay a scanned check ---

    /** Open a check to collect [request.amountMinor]. The key IS the check id, so a retry returns the same check. */
    suspend fun createCheck(request: CreateCheckRequest, idempotencyKey: String): ApiOutcome<CheckDto> =
        call(post("/v1/checks", CreateCheckRequest.serializer(), request, idempotencyKey = idempotencyKey), CheckDto.serializer())

    /** A check by id: the merchant's own, one this user paid, or any check still open (the QR preview). */
    suspend fun check(id: String): ApiOutcome<CheckDto> =
        call(get("/v1/checks/${id.urlEncode()}"), CheckDto.serializer())

    suspend fun cancelCheck(id: String): ApiOutcome<CheckDto> =
        call(post("/v1/checks/${id.urlEncode()}/cancel", Unit.serializer(), Unit), CheckDto.serializer())

    /**
     * Settle a check from the caller's wallet. Only ever called by the
     * PaymentSubmitter, with a persisted key — same contract as [transfer]; the
     * response carries `transaction_id` + `status` like a transfer does.
     */
    suspend fun payCheck(checkId: String, request: PayCheckRequest, idempotencyKey: String): ApiOutcome<PostResponse> =
        call(
            post("/v1/checks/${checkId.urlEncode()}/pay", PayCheckRequest.serializer(), request, idempotencyKey = idempotencyKey),
            PostResponse.serializer(),
        )

    // --- Request building ---

    /** Typed request tag: carries no bearer and must never enter the refresh path. */
    private object NoAuth

    private fun <T> post(
        path: String,
        serializer: KSerializer<T>,
        payload: T,
        authed: Boolean = true,
        idempotencyKey: String? = null,
    ): Request = Request.Builder()
        .url(baseUrl + path)
        .post(json.encodeToString(serializer, payload).toRequestBody(jsonMedia))
        .apply {
            if (!authed) tag(NoAuth::class.java, NoAuth)
            if (idempotencyKey != null) header("Idempotency-Key", idempotencyKey)
        }
        .build()

    private fun get(path: String): Request = Request.Builder().url(baseUrl + path).get().build()

    // --- Execution (everything — I/O and JSON decode — off the caller's thread) ---

    private suspend fun <T> call(request: Request, serializer: KSerializer<T>): ApiOutcome<T> =
        withContext(Dispatchers.IO) {
            try {
                client.newCall(request).execute().use { resp ->
                    val body = resp.body?.string().orEmpty()
                    if (resp.isSuccessful) decode(resp.code, body, serializer) else failure(resp.code, body)
                }
            } catch (e: IOException) {
                ApiOutcome.Offline(e)
            }
        }

    private suspend fun callIgnoringBody(request: Request): ApiOutcome<Unit> =
        withContext(Dispatchers.IO) {
            try {
                client.newCall(request).execute().use { resp ->
                    val body = resp.body?.string().orEmpty()
                    if (resp.isSuccessful) ApiOutcome.Ok(Unit) else failure(resp.code, body)
                }
            } catch (e: IOException) {
                ApiOutcome.Offline(e)
            }
        }

    /**
     * A 2xx we cannot read becomes `Failed` with the 2xx status — callers that
     * move money (PaymentSubmitter) treat that as "accepted, answer lost", never
     * as a refusal.
     */
    private fun <T> decode(status: Int, body: String, serializer: KSerializer<T>): ApiOutcome<T> = try {
        if (body.isBlank()) {
            ApiOutcome.Failed(ErrorCode.UNKNOWN, status, "empty response body")
        } else {
            ApiOutcome.Ok(json.decodeFromString(serializer, body))
        }
    } catch (e: Exception) {
        ApiOutcome.Failed(ErrorCode.UNKNOWN, status, "malformed response: ${e.message}")
    }

    private fun failure(status: Int, body: String): ApiOutcome.Failed {
        val error = parseEnvelope(body)?.error
        val code = error?.code?.let { ErrorCode.fromWire(it) } ?: when (status) {
            401 -> ErrorCode.UNAUTHORIZED
            403 -> ErrorCode.FORBIDDEN
            404 -> ErrorCode.NOT_FOUND
            409 -> ErrorCode.CONFLICT
            422 -> ErrorCode.LIMIT_EXCEEDED
            429 -> ErrorCode.RATE_LIMITED
            else -> ErrorCode.UNKNOWN
        }
        return ApiOutcome.Failed(code, status, error?.message, error?.requestId)
    }

    private fun parseEnvelope(body: String): ErrorEnvelope? = try {
        if (body.isBlank()) null else json.decodeFromString(ErrorEnvelope.serializer(), body)
    } catch (_: Exception) {
        null
    }

    // --- Token lifecycle ---

    private val refreshLock = Any()

    /**
     * Single-flight rotation. [staleToken] is the access token the caller holds
     * (null if none): when the session already holds a *different* one, another
     * caller rotated while we waited for the lock and we simply reuse it.
     * Blocking; call from an I/O thread. Applies the outcome to the session.
     */
    private fun ensureFreshToken(staleToken: String?): RefreshOutcome {
        synchronized(refreshLock) {
            val current = session.accessToken()
            if (current != null && current != staleToken) return RefreshOutcome.Refreshed(current)
            val refreshToken = session.refreshToken() ?: return RefreshOutcome.Dead
            return when (val result = refreshBlocking(refreshToken)) {
                is RefreshResult.Rotated -> {
                    adoptTokens(result.tokens)
                    RefreshOutcome.Refreshed(result.tokens.accessToken)
                }
                RefreshResult.Dead -> {
                    session.clear()
                    RefreshOutcome.Dead
                }
                is RefreshResult.Unreachable -> RefreshOutcome.Unreachable(result.cause)
            }
        }
    }

    private sealed interface RefreshResult {
        data class Rotated(val tokens: TokenResponse) : RefreshResult
        data object Dead : RefreshResult
        data class Unreachable(val cause: IOException) : RefreshResult
    }

    /** Blocking refresh on the bare client — safe inside the Authenticator (no recursion). */
    private fun refreshBlocking(refreshToken: String): RefreshResult {
        val request = post("/v1/auth/refresh", RefreshRequest.serializer(), RefreshRequest(refreshToken), authed = false)
        return try {
            bareClient.newCall(request).execute().use { resp ->
                val body = resp.body?.string().orEmpty()
                when {
                    resp.code == 401 || resp.code == 403 -> RefreshResult.Dead
                    !resp.isSuccessful -> RefreshResult.Unreachable(IOException("refresh answered HTTP ${resp.code}"))
                    else -> try {
                        RefreshResult.Rotated(json.decodeFromString(TokenResponse.serializer(), body))
                    } catch (e: Exception) {
                        // A malformed 200 used to escape the authenticator as a
                        // SerializationException and crash the call.
                        RefreshResult.Unreachable(IOException("refresh answered an unreadable body", e))
                    }
                }
            }
        } catch (e: IOException) {
            RefreshResult.Unreachable(e)
        }
    }

    private var refreshJob: Job? = null

    /**
     * Rotate at ~80% of the lifetime so the first request after idle finds a
     * fresh token. Waits for the app to be on screen (a frozen/background app
     * does no rotation; the interceptor's staleness check covers its return).
     */
    private fun scheduleProactiveRefresh(accessToken: String, expiresInSeconds: Long) {
        synchronized(refreshLock) {
            refreshJob?.cancel()
            if (expiresInSeconds <= 0) {
                refreshJob = null
                return
            }
            refreshJob = scope.launch {
                delay(expiresInSeconds * 1000L * PROACTIVE_AT_PERCENT / 100)
                foreground.first { it }
                if (session.accessToken() != accessToken) return@launch // already rotated
                // Unreachable is ignored here: the interceptor / 401 path retries
                // on the next real request, and Dead has already cleared the session.
                withContext(Dispatchers.IO) { ensureFreshToken(accessToken) }
            }
        }
    }

    /**
     * Adds `Authorization: Bearer <access>` unless the request is tagged
     * [NoAuth]; a token past ~80% of its lifetime is rotated first.
     */
    private inner class AuthHeaderInterceptor : Interceptor {
        override fun intercept(chain: Interceptor.Chain): Response {
            val request = chain.request()
            if (request.tag(NoAuth::class.java) != null) return chain.proceed(request)

            var token = session.accessToken()
            if (token != null && session.accessTokenStale(clock())) {
                when (val fresh = ensureFreshToken(token)) {
                    is RefreshOutcome.Refreshed -> token = fresh.accessToken
                    // The session is gone; answer as the server would, without a
                    // pointless round trip (the authenticator then finds no
                    // refresh token and lets the 401 through).
                    RefreshOutcome.Dead -> return syntheticUnauthorized(request)
                    // Surfaces from execute() as Offline: the server was unreachable.
                    is RefreshOutcome.Unreachable -> throw fresh.cause
                }
            }
            return chain.proceed(if (token != null) request.withBearer(token) else request)
        }
    }

    /**
     * On a 401, mint a fresh access token from the refresh token and retry once.
     * Single-flight via [ensureFreshToken]. A refresh the server refuses (401/403)
     * wipes the session so the UI falls back to password login; one that cannot
     * be completed throws an [IOException] — allowed by the Authenticator
     * contract — so the call fails as *offline* with the session intact.
     */
    private inner class RefreshAuthenticator : Authenticator {
        @Throws(IOException::class)
        override fun authenticate(route: Route?, response: Response): Request? {
            // login/register/refresh/logout: a 401 is the answer, not a stale token.
            if (response.request.tag(NoAuth::class.java) != null) return null
            if (responseCount(response) >= 2) return null // already retried once

            val failedToken = response.request.header("Authorization")?.removePrefix("Bearer ")
            return when (val fresh = ensureFreshToken(failedToken)) {
                is RefreshOutcome.Refreshed -> response.request.withBearer(fresh.accessToken)
                RefreshOutcome.Dead -> null
                is RefreshOutcome.Unreachable -> throw IOException("token refresh unreachable", fresh.cause)
            }
        }

        private fun responseCount(response: Response): Int {
            var count = 1
            var prior = response.priorResponse
            while (prior != null) {
                count++
                prior = prior.priorResponse
            }
            return count
        }
    }

    private fun syntheticUnauthorized(request: Request): Response = Response.Builder()
        .request(request)
        .protocol(Protocol.HTTP_1_1)
        .code(401)
        .message("session expired")
        .body("".toResponseBody(null))
        .build()

    private fun Request.withBearer(token: String): Request =
        newBuilder().header("Authorization", "Bearer $token").build()

    private companion object {
        const val PROACTIVE_AT_PERCENT = 80
    }
}

private fun String.urlEncode(): String =
    java.net.URLEncoder.encode(this, "UTF-8")
