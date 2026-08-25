package tj.payment.wallet.data

import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import okhttp3.Authenticator
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.Route
import tj.payment.core.ApiOutcome
import tj.payment.core.CredentialsRequest
import tj.payment.core.ErrorCode
import tj.payment.core.ErrorEnvelope
import tj.payment.core.map
import tj.payment.core.RefreshRequest
import tj.payment.core.ResolveResponse
import tj.payment.core.TokenResponse
import tj.payment.core.WalletDto

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
        postJson("/v1/auth/register", CredentialsRequest(phone, password), authed = false)
            .decode(TokenResponse.serializer())

    suspend fun login(phone: String, password: String): ApiOutcome<TokenResponse> =
        postJson("/v1/auth/login", CredentialsRequest(phone, password), authed = false)
            .decode(TokenResponse.serializer())

    // --- Authenticated endpoints ---

    suspend fun logout(refreshToken: String): ApiOutcome<Unit> =
        postJson("/v1/auth/logout", RefreshRequest(refreshToken), authed = false)
            .map { }

    suspend fun wallets(): ApiOutcome<List<WalletDto>> =
        get("/v1/wallets", authed = true)
            .decode(kotlinx.serialization.builtins.ListSerializer(WalletDto.serializer()))

    /** The "check number" / QR-scan lookup used by the send flow. */
    suspend fun resolveByPhone(phone: String): ApiOutcome<ResolveResponse> =
        get("/v1/users/resolve?phone=${phone.urlEncode()}", authed = true)
            .decode(ResolveResponse.serializer())

    suspend fun resolveByWallet(walletId: String): ApiOutcome<ResolveResponse> =
        get("/v1/users/resolve?wallet=${walletId.urlEncode()}", authed = true)
            .decode(ResolveResponse.serializer())

    // --- Plumbing ---

    private data class Raw(val status: Int, val body: String)

    private suspend fun postJson(
        path: String,
        body: Any,
        authed: Boolean,
    ): ApiOutcome<Raw> {
        val payload = when (body) {
            is CredentialsRequest -> json.encodeToString(CredentialsRequest.serializer(), body)
            is RefreshRequest -> json.encodeToString(RefreshRequest.serializer(), body)
            else -> error("unsupported body type: ${body::class}")
        }
        val request = Request.Builder()
            .url(baseUrl + path)
            .post(payload.toRequestBody(jsonMedia))
            .apply { if (!authed) header(NO_AUTH_HEADER, "1") }
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
                session.persist(newTokens.userId, newTokens.refreshToken)
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
