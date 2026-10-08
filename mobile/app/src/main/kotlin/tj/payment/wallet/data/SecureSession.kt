package tj.payment.wallet.data

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import android.content.Context
import tj.payment.core.LocalStorageException

/**
 * The session store. The long-lived **refresh token** is persisted in
 * Keystore-backed `EncryptedSharedPreferences` (its own file and master key —
 * see [SecureStore]); a raw DB/file dump yields no usable token. The
 * short-lived **access token** lives only in memory: it dies with the process,
 * and is re-minted from the refresh token on next launch, so it never sits on
 * disk at all.
 *
 * Every write is synchronous (`commit`): a rotated refresh token that was only
 * queued (`apply`) and then lost to a process kill would sign the user out, the
 * server having already invalidated the old one. Writes are therefore called
 * off the main thread (ApiClient's I/O threads, AuthRepository's IO dispatcher).
 *
 * Reads that hit a Keystore/crypto fault throw [LocalStorageException], which
 * ApiClient maps to a typed outcome — never mistaken for "no session".
 */
class SecureSession(context: Context) : SessionStore {

    private val store = SecureStore(
        context,
        fileName = "session.v2.secure",
        keyAlias = "tj.payment.wallet.mk.session",
        legacyFileName = "session.secure",
    )
    private val prefs get() = store.opened.prefs

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

    /** @throws LocalStorageException on a Keystore/crypto fault. */
    override fun refreshToken(): String? = secureIo { prefs.getString(KEY_REFRESH, null) }

    /** The signed-in user (owner of pending payments). @throws LocalStorageException on a fault. */
    fun userId(): String? = secureIo { prefs.getString(KEY_USER, null) }

    /** Display only; a fault reads as "unknown". */
    fun phone(): String? = try {
        secureIo { prefs.getString(KEY_PHONE, null) }
    } catch (_: LocalStorageException) {
        null
    }

    /** A fault reads as "no session" here only: the caller then routes to password sign-in. */
    fun hasPersistedSession(): Boolean = try {
        refreshToken() != null
    } catch (_: LocalStorageException) {
        false
    }

    /** Synchronous; blocking — off the main thread. @throws LocalStorageException on a fault. */
    override fun persistTokens(userId: String, refreshToken: String) {
        secureIo {
            prefs.edit()
                .putString(KEY_USER, userId)
                .putString(KEY_REFRESH, refreshToken)
                .commit()
        }
    }

    /** The signed-in phone number (digits-only) — set at login/register, shown on
     * the Receive screen so the user can tell a sender what to type. */
    fun persistPhone(phone: String) {
        secureIo { prefs.edit().putString(KEY_PHONE, phone).commit() }
    }

    /**
     * Wipe the session. The in-memory token and the signed-out signal never
     * depend on the disk write: even if the Keystore faults here, this process
     * is signed out at once.
     */
    override fun clear() {
        accessTokenInMemory = null
        staleAtMs = Long.MAX_VALUE
        try {
            secureIo { prefs.edit().clear().commit() }
        } catch (_: LocalStorageException) {
            // Best effort; the server-side logout (and token expiry) still apply.
        }
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
