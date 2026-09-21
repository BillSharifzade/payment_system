package tj.payment.wallet.data

import tj.payment.core.ApiOutcome
import tj.payment.core.ErrorCode
import tj.payment.core.TokenResponse

/**
 * The one place session state is written. Screens never touch [SecureSession] or
 * [ApiClient] directly — they go through here, so the "store tokens on success"
 * and "restore on launch" rules can't be forgotten at a call site.
 */
class AuthRepository(
    private val api: ApiClient,
    private val session: SecureSession,
) {
    fun hasPersistedSession(): Boolean = session.hasPersistedSession()

    suspend fun login(phone: String, password: String): ApiOutcome<Unit> =
        adopt(api.login(phone, password), phone)

    suspend fun register(phone: String, password: String): ApiOutcome<Unit> =
        adopt(api.register(phone, password), phone)

    /**
     * Re-establish a session at launch in ONE request: `POST /v1/auth/refresh`
     * with the persisted refresh token (no probing an authed endpoint into a
     * 401 first, no discarded payload — Home fetches its own data next). The
     * server refusing the token (401/403) means the session is dead: it is
     * cleared and the caller routes to login. Anything else — offline, 5xx, an
     * unreadable answer — keeps the session so a flaky network never signs the
     * user out; the caller lands on Home in its offline state.
     */
    suspend fun restore(): ApiOutcome<Unit> {
        if (!session.hasPersistedSession()) {
            return ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, "no session")
        }
        if (session.accessToken() != null) return ApiOutcome.Ok(Unit)
        return when (val outcome = api.refreshSession()) {
            is ApiClient.RefreshOutcome.Refreshed -> ApiOutcome.Ok(Unit)
            ApiClient.RefreshOutcome.Dead -> ApiOutcome.Failed(ErrorCode.UNAUTHORIZED, 401, "session revoked")
            is ApiClient.RefreshOutcome.Unreachable -> ApiOutcome.Offline(outcome.cause)
        }
    }

    /**
     * Local sign-out is immediate — the session is wiped (which flips
     * `signedOut`, so the UI leaves Home at once) BEFORE the best-effort
     * server-side revocation, which may be slow or impossible offline. A
     * revocation that never arrives costs nothing: the refresh token is gone
     * from the device either way, and the server expires it on its own.
     */
    suspend fun logout() {
        val refreshToken = session.refreshToken()
        session.clear()
        if (refreshToken != null) api.logout(refreshToken)
    }

    private fun adopt(outcome: ApiOutcome<TokenResponse>, phone: String): ApiOutcome<Unit> = when (outcome) {
        is ApiOutcome.Ok -> {
            api.adoptTokens(outcome.value)
            session.persistPhone(phone)
            ApiOutcome.Ok(Unit)
        }
        is ApiOutcome.Failed -> outcome
        is ApiOutcome.Offline -> outcome
    }
}
