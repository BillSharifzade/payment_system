package tj.payment.wallet.data

import tj.payment.core.ApiOutcome
import tj.payment.core.TokenResponse
import tj.payment.core.WalletDto

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
        adopt(api.login(phone, password))

    suspend fun register(phone: String, password: String): ApiOutcome<Unit> =
        adopt(api.register(phone, password))

    /**
     * Re-establish a session at launch from the persisted refresh token. With no
     * in-memory access token, this wallets probe returns 401, which drives the
     * ApiClient's refresh path: success mints a fresh access token (we're signed
     * in); a 401 that survives means the refresh token is dead (cleared → login).
     * Offline leaves the session intact so a flaky network doesn't sign the user
     * out.
     */
    suspend fun restore(): ApiOutcome<Unit> {
        if (session.refreshToken() == null) {
            return ApiOutcome.Failed(tj.payment.core.ErrorCode.UNAUTHORIZED, 401, "no session")
        }
        return when (val probe = api.wallets()) {
            is ApiOutcome.Ok -> ApiOutcome.Ok(Unit)
            is ApiOutcome.Offline -> ApiOutcome.Offline(probe.cause)
            is ApiOutcome.Failed -> {
                if (probe.httpStatus == 401) session.clear()
                ApiOutcome.Failed(probe.code, probe.httpStatus, probe.serverMessage)
            }
        }
    }

    suspend fun wallets(): ApiOutcome<List<WalletDto>> = api.wallets()

    suspend fun logout() {
        session.refreshToken()?.let { api.logout(it) }
        session.clear()
    }

    private fun adopt(outcome: ApiOutcome<TokenResponse>): ApiOutcome<Unit> = when (outcome) {
        is ApiOutcome.Ok -> {
            val t = outcome.value
            session.setAccessToken(t.accessToken)
            session.persist(t.userId, t.refreshToken)
            ApiOutcome.Ok(Unit)
        }
        is ApiOutcome.Failed -> ApiOutcome.Failed(outcome.code, outcome.httpStatus, outcome.serverMessage)
        is ApiOutcome.Offline -> ApiOutcome.Offline(outcome.cause)
    }
}
