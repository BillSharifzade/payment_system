package tj.payment.wallet.data

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The session store. The long-lived **refresh token** is persisted in
 * Keystore-backed `EncryptedSharedPreferences` — a raw DB/file dump yields no
 * usable token. The short-lived **access token** lives only in memory: it dies
 * with the process, and is re-minted from the refresh token on next launch, so
 * it never sits on disk at all.
 *
 * Opening the store is guarded (see [SecurePrefs]): a corrupt file is wiped and
 * recreated instead of crashing the launch; the user then signs in again.
 */
class SecureSession(context: Context) : SessionStore {

    private val opened = SecurePrefs.open(context, "session.secure")
    private val prefs get() = opened.prefs

    @Volatile
    private var accessTokenInMemory: String? = null

    /** Epoch ms after which the access token should be rotated before use. */
    @Volatile
    private var staleAtMs: Long = Long.MAX_VALUE

    private val _signedOut = MutableStateFlow(false)

    /**
     * Flips to true inside [clear] — an explicit logout, or a refresh answered
     * "dead" from any thread (the OkHttp authenticator, the proactive-refresh
     * timer). AppRoot observes it once and routes to Login; the observer calls
     * [consumeSignedOut] after navigating so a later sign-out fires again.
     */
    val signedOut: StateFlow<Boolean> = _signedOut.asStateFlow()

    fun consumeSignedOut() {
        _signedOut.value = false
    }

    override fun accessToken(): String? = accessTokenInMemory

    override fun accessTokenStale(nowMs: Long): Boolean =
        accessTokenInMemory != null && nowMs >= staleAtMs

    override fun setAccessToken(token: String?, expiresInSeconds: Long, nowMs: Long) {
        accessTokenInMemory = token
        staleAtMs = if (token != null && expiresInSeconds > 0) {
            nowMs + expiresInSeconds * 1000L * STALE_AT_PERCENT / 100
        } else {
            Long.MAX_VALUE
        }
    }

    override fun refreshToken(): String? = prefs.getString(KEY_REFRESH, null)
    fun userId(): String? = prefs.getString(KEY_USER, null)
    fun phone(): String? = prefs.getString(KEY_PHONE, null)
    fun hasPersistedSession(): Boolean = refreshToken() != null

    override fun persistTokens(userId: String, refreshToken: String) {
        prefs.edit()
            .putString(KEY_USER, userId)
            .putString(KEY_REFRESH, refreshToken)
            .apply()
    }

    /** The signed-in phone number (digits-only) — set at login/register, shown on
     * the Receive screen so the user can tell a sender what to type. */
    fun persistPhone(phone: String) {
        prefs.edit().putString(KEY_PHONE, phone).apply()
    }

    override fun clear() {
        accessTokenInMemory = null
        staleAtMs = Long.MAX_VALUE
        prefs.edit().clear().apply()
        _signedOut.value = true
    }

    private companion object {
        const val KEY_REFRESH = "refresh_token"
        const val KEY_USER = "user_id"
        const val KEY_PHONE = "phone"

        /** Rotate at 80% of the lifetime — well clear of clock skew and a slow request. */
        const val STALE_AT_PERCENT = 80
    }
}
