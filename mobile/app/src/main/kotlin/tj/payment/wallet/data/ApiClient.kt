package tj.payment.wallet.data

import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import okhttp3.Authenticator
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.Route
import tj.payment.core.AccountResponse
import tj.payment.core.ApiOutcome
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
import tj.payment.core.PostResponse
import tj.payment.core.RefreshRequest
import tj.payment.core.ResolveResponse
import tj.payment.core.StatementResponse
import tj.payment.core.SubmitKycRequest
import tj.payment.core.TokenResponse
import tj.payment.core.TransferRequest
import tj.payment.core.WalletDto
import tj.payment.core.map

/**
 * The single HTTP boundary to the payment backend. Thin on purpose: OkHttp +
 * kotlinx-serialization, no Retrofit proxies or reflection — fast, small, and
 * every request path is explicit and auditable.
 *
 * Two cross-cutting behaviours live here so no call site can forget them:
 *  - an interceptor attaches the Bearer access token;
 *  - an [Authenticator] transparently refreshes a rotated/expired token on a
 *    401, single-flight, and retries the original request once.
 */
class ApiClient(
    private val baseUrl: String,
    private val session: SecureSession,
) {
    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }
    private val jsonMedia = "application/json; charset=utf-8".toMediaType()

    private val client: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(20, TimeUnit.SECONDS)
        .addInterceptor(AuthHeaderInterceptor(session))
        .authenticator(RefreshAuthenticator())
        .build()

    // --- Public endpoints (no bearer) ---

    suspend fun register(phone: String, password: String): ApiOutcome<TokenResponse> =
        postJson(
            "/v1/auth/register",
            json.encodeToString(CredentialsRequest.serializer(), CredentialsRequest(phone, password)),
            authed = false,
        ).decode(TokenResponse.serializer())

    suspend fun login(phone: String, password: String): ApiOutcome<TokenResponse> =
        postJson(
            "/v1/auth/login",
            json.encodeToString(CredentialsRequest.serializer(), CredentialsRequest(phone, password)),
            authed = false,
        ).decode(TokenResponse.serializer())

    suspend fun logout(refreshToken: String): ApiOutcome<Unit> =
        postJson(
            "/v1/auth/logout",
            json.encodeToString(RefreshRequest.serializer(), RefreshRequest(refreshToken)),
            authed = false,
        ).map { }

    // --- Authenticated endpoints ---

    suspend fun wallets(): ApiOutcome<List<WalletDto>> =
        get("/v1/wallets", authed = true)
            .decode(kotlinx.serialization.builtins.ListSerializer(WalletDto.serializer()))

    suspend fun createWallet(currency: String): ApiOutcome<AccountResponse> =
        postJson(
            "/v1/wallets",
            json.encodeToString(CreateWalletRequest.serializer(), CreateWalletRequest(currency)),
            authed = true,
        ).decode(AccountResponse.serializer())

    suspend fun config(): ApiOutcome<ClientConfigResponse> =
        get("/v1/config", authed = true)
            .decode(ClientConfigResponse.serializer())

    /** The "check number" / QR-scan lookup used by the send flow. */
    suspend fun resolveByPhone(phone: String): ApiOutcome<ResolveResponse> =
        get("/v1/users/resolve?phone=${phone.urlEncode()}", authed = true)
            .decode(ResolveResponse.serializer())

    suspend fun resolveByWallet(walletId: String): ApiOutcome<ResolveResponse> =
        get("/v1/users/resolve?wallet=${walletId.urlEncode()}", authed = true)
            .decode(ResolveResponse.serializer())

    /** One page of an account's statement, newest first. */
    suspend fun statement(
        accountId: String,
        cursor: String?,
        limit: Int,
    ): ApiOutcome<StatementResponse> {
        val query = buildString {
            append("?limit=").append(limit)
            if (cursor != null) append("&cursor=").append(cursor.urlEncode())
        }
        return get("/v1/accounts/${accountId.urlEncode()}/transactions$query", authed = true)
            .decode(StatementResponse.serializer())
    }

    /** Post a transfer. [idempotencyKey] comes from the PaymentSubmitter — the
     * same key MUST be resent on retries so the server charges at most once. */
    suspend fun transfer(request: TransferRequest, idempotencyKey: String): ApiOutcome<PostResponse> =
        postJson(
            "/v1/transfers",
            json.encodeToString(TransferRequest.serializer(), request),
            authed = true,
            idempotencyKey = idempotencyKey,
        ).decode(PostResponse.serializer())

    suspend fun kycStatus(): ApiOutcome<KycStatusResponse> =
        get("/v1/kyc", authed = true)
            .decode(KycStatusResponse.serializer())

    /** Upload an identity document (multipart field `file`). */
    suspend fun uploadKycDocument(bytes: ByteArray, mimeType: String): ApiOutcome<DocumentResponse> {
        val body = MultipartBody.Builder()
            .setType(MultipartBody.FORM)
            .addFormDataPart("file", "document", bytes.toRequestBody(mimeType.toMediaType()))
            .build()
        val request = Request.Builder().url("$baseUrl/v1/kyc/documents").post(body).build()
        return execute(request).decode(DocumentResponse.serializer())
    }

    suspend fun submitKyc(request: SubmitKycRequest): ApiOutcome<tj.payment.core.KycSubmissionDto> =
        postJson(
            "/v1/kyc/submissions",
            json.encodeToString(SubmitKycRequest.serializer(), request),
            authed = true,
        ).decode(tj.payment.core.KycSubmissionDto.serializer())

    suspend fun fxRates(): ApiOutcome<List<FxRateDto>> =
        get("/v1/fx/rates", authed = true)
            .decode(kotlinx.serialization.builtins.ListSerializer(FxRateDto.serializer()))

    /** Convert between the caller's own wallets. Same idempotency contract as [transfer]. */
    suspend fun fx(request: FxRequest, idempotencyKey: String): ApiOutcome<FxResponse> =
        postJson(
            "/v1/fx",
            json.encodeToString(FxRequest.serializer(), request),
            authed = true,
            idempotencyKey = idempotencyKey,
        ).decode(FxResponse.serializer())

    // --- Plumbing ---

    private data class Raw(val status: Int, val body: String)

    private suspend fun postJson(
        path: String,
        payload: String,
        authed: Boolean,
        idempotencyKey: String? = null,
    ): ApiOutcome<Raw> {
        val request = Request.Builder()
            .url(baseUrl + path)
            .post(payload.toRequestBody(jsonMedia))
            .apply {
                if (!authed) header(NO_AUTH_HEADER, "1")
                if (idempotencyKey != null) header("Idempotency-Key", idempotencyKey)
            }
            .build()
        return execute(request)
    }

    private suspend fun get(path: String, authed: Boolean): ApiOutcome<Raw> {
        val request = Request.Builder()
            .url(baseUrl + path)
            .get()
            .apply { if (!authed) header(NO_AUTH_HEADER, "1") }
            .build()
        return execute(request)
    }

    private suspend fun execute(request: Request): ApiOutcome<Raw> = withContext(Dispatchers.IO) {
        try {
            client.newCall(request).execute().use { resp ->
                val bodyStr = resp.body?.string().orEmpty()
                if (resp.isSuccessful) {
                    ApiOutcome.Ok(Raw(resp.code, bodyStr))
                } else {
                    ApiOutcome.Failed(errorCodeOf(resp.code, bodyStr), resp.code, messageOf(bodyStr))
                }
            }
        } catch (e: IOException) {
            ApiOutcome.Offline(e)
        }
    }

    private fun <T> ApiOutcome<Raw>.decode(
        serializer: kotlinx.serialization.KSerializer<T>,
    ): ApiOutcome<T> = when (this) {
        is ApiOutcome.Ok -> try {
            if (value.body.isBlank()) {
                ApiOutcome.Failed(ErrorCode.UNKNOWN, value.status, "empty response body")
            } else {
                ApiOutcome.Ok(json.decodeFromString(serializer, value.body))
            }
        } catch (e: Exception) {
            ApiOutcome.Failed(ErrorCode.UNKNOWN, value.status, "malformed response: ${e.message}")
        }
        is ApiOutcome.Failed -> this
        is ApiOutcome.Offline -> this
    }

    private fun errorCodeOf(status: Int, body: String): ErrorCode {
        parseEnvelope(body)?.error?.code?.let { return ErrorCode.fromWire(it) }
        return when (status) {
            401 -> ErrorCode.UNAUTHORIZED
            403 -> ErrorCode.FORBIDDEN
            404 -> ErrorCode.NOT_FOUND
            409 -> ErrorCode.CONFLICT
            422 -> ErrorCode.LIMIT_EXCEEDED
            429 -> ErrorCode.RATE_LIMITED
            else -> ErrorCode.UNKNOWN
        }
    }

    private fun messageOf(body: String): String? = parseEnvelope(body)?.error?.message

    private fun parseEnvelope(body: String): ErrorEnvelope? = try {
        if (body.isBlank()) null else json.decodeFromString(ErrorEnvelope.serializer(), body)
    } catch (_: Exception) {
        null
    }

    /** Adds `Authorization: Bearer <access>` unless the request opts out. */
    private class AuthHeaderInterceptor(private val session: SecureSession) : Interceptor {
        override fun intercept(chain: Interceptor.Chain): Response {
            val original = chain.request()
            if (original.header(NO_AUTH_HEADER) != null) {
                return chain.proceed(original.newBuilder().removeHeader(NO_AUTH_HEADER).build())
            }
            val token = session.accessToken()
            val request = if (token != null) {
                original.newBuilder().header("Authorization", "Bearer $token").build()
            } else {
                original
            }
            return chain.proceed(request)
        }
    }

    /**
     * On a 401, mint a fresh access token from the refresh token and retry once.
     * Single-flight: concurrent 401s serialize on [refreshLock]; a request that
     * arrives after another thread already refreshed simply reuses the new token.
     * A refresh that itself fails (revoked family / expired) wipes the session so
     * the UI falls back to password login.
     */
    private inner class RefreshAuthenticator : Authenticator {
        private val refreshLock = Any()

        override fun authenticate(route: Route?, response: Response): Request? {
            if (priorResponseCount(response) >= 2) return null // already retried once
            val tokenThatFailed = authTokenOf(response.request)

            synchronized(refreshLock) {
                val current = session.accessToken()
                if (current != null && current != tokenThatFailed) {
                    // Another thread refreshed while we waited; just use it.
                    return response.request.withBearer(current)
                }
                val refresh = session.refreshToken() ?: return null
                val newTokens = refreshBlocking(refresh) ?: run {
                    session.clear()
                    return null
                }
                session.setAccessToken(newTokens.accessToken)
                session.persistTokens(newTokens.userId, newTokens.refreshToken)
                return response.request.withBearer(newTokens.accessToken)
            }
        }

        private fun authTokenOf(request: Request): String? =
            request.header("Authorization")?.removePrefix("Bearer ")

        private fun priorResponseCount(response: Response): Int {
            var count = 1
            var prior = response.priorResponse
            while (prior != null) {
                count++
                prior = prior.priorResponse
            }
            return count
        }
    }

    /** Blocking refresh on a bare client (no auth/authenticator) — safe to call
     * from inside the Authenticator without recursion. */
    private fun refreshBlocking(refreshToken: String): TokenResponse? {
        val bareClient = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(20, TimeUnit.SECONDS)
            .build()
        val payload = json.encodeToString(RefreshRequest.serializer(), RefreshRequest(refreshToken))
        val request = Request.Builder()
            .url("$baseUrl/v1/auth/refresh")
            .post(payload.toRequestBody(jsonMedia))
            .build()
        return try {
            bareClient.newCall(request).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val body = resp.body?.string().orEmpty()
                if (body.isBlank()) null
                else json.decodeFromString(TokenResponse.serializer(), body)
            }
        } catch (_: IOException) {
            null
        }
    }

    private fun Request.withBearer(token: String): Request =
        newBuilder().header("Authorization", "Bearer $token").build()

    private companion object {
        const val NO_AUTH_HEADER = "X-No-Auth"
    }
}

private fun String.urlEncode(): String =
    java.net.URLEncoder.encode(this, "UTF-8")
