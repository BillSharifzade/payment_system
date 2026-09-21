package tj.payment.wallet.data

/**
 * What the HTTP layer needs from the session, and nothing more. An interface so
 * [ApiClient] unit-tests on the JVM against an in-memory fake — the real
 * [SecureSession] is Keystore-backed and only exists on a device.
 */
interface SessionStore {
    /** The short-lived bearer, in memory only. */
    fun accessToken(): String?

    /**
     * True once the in-memory token is past ~80% of its lifetime: rotate it
     * before use instead of paying a 401 round trip. An unknown lifetime
     * (`expires_in` absent or 0) is never stale — the 401 path still works.
     */
    fun accessTokenStale(nowMs: Long): Boolean

    /** [expiresInSeconds] <= 0 means the lifetime is unknown. */
    fun setAccessToken(token: String?, expiresInSeconds: Long, nowMs: Long)

    /** The long-lived rotating token, persisted encrypted. */
    fun refreshToken(): String?

    /** Persist the rotating half of a session (login and every rotation). */
    fun persistTokens(userId: String, refreshToken: String)

    /** Wipe everything — logout, or a refresh answered "dead" (revoked family). */
    fun clear()
}
