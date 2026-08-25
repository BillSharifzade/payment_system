package tj.payment.wallet.data

import android.content.Context
import android.content.SharedPreferences
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

/**
 * The session store. The long-lived **refresh token** is persisted in
 * Keystore-backed [EncryptedSharedPreferences] — a raw DB/file dump yields no
 * usable token. The short-lived **access token** lives only in memory: it dies
 * with the process, and is re-minted from the refresh token on next launch, so
 * it never sits on disk at all.
 */
class SecureSession(context: Context) {

    private val prefs: SharedPreferences = run {
        val masterKey = MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build()
        EncryptedSharedPreferences.create(
            context,
            "session.secure",
            masterKey,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    @Volatile
    private var accessTokenInMemory: String? = null

    fun accessToken(): String? = accessTokenInMemory
    fun setAccessToken(token: String?) {
        accessTokenInMemory = token
    }

    fun refreshToken(): String? = prefs.getString(KEY_REFRESH, null)
    fun userId(): String? = prefs.getString(KEY_USER, null)
    fun hasPersistedSession(): Boolean = refreshToken() != null

    /** Persist the durable half of a session (after login or a token rotation). */
    fun persist(userId: String, refreshToken: String) {
        prefs.edit()
            .putString(KEY_USER, userId)
            .putString(KEY_REFRESH, refreshToken)
            .apply()
    }

    /** Wipe everything — logout, or a refresh rejected as a revoked family. */
    fun clear() {
        accessTokenInMemory = null
        prefs.edit().clear().apply()
    }

    private companion object {
        const val KEY_REFRESH = "refresh_token"
        const val KEY_USER = "user_id"
    }
}
