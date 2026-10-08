package tj.payment.core

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * When the app must be unlocked with the device's own authentication before
 * showing anything (FRONTEND.md §2.3: "App-lock layer gates the UI
 * independently of tokens"). Pure timing logic; the app supplies a
 * **monotonic** clock (`SystemClock.elapsedRealtime`) so changing the wall
 * clock cannot skip the lock — and a clock that runs backwards locks.
 *
 * Locks: at launch with a persisted session ([lock]), and on returning to the
 * foreground after [timeoutMs] or more in the background. Unlocks: a successful
 * device authentication, a fresh password sign-in, or sign-out (nothing left
 * to protect).
 */
class AppLockPolicy(
    private val clock: () -> Long,
    val timeoutMs: Long = DEFAULT_TIMEOUT_MS,
) {
    private val _locked = MutableStateFlow(false)
    val locked: StateFlow<Boolean> = _locked.asStateFlow()

    private var backgroundSinceMs: Long? = null

    @Synchronized
    fun lock() {
        _locked.value = true
    }

    @Synchronized
    fun unlock() {
        _locked.value = false
    }

    @Synchronized
    fun onBackground() {
        if (backgroundSinceMs == null) backgroundSinceMs = clock()
    }

    /**
     * [hasSession]: whether a signed-in session exists (otherwise there is
     * nothing to lock). Lazy: only asked when the time away calls for a lock,
     * so a quick app switch never touches secure storage.
     */
    @Synchronized
    fun onForeground(hasSession: () -> Boolean) {
        val since = backgroundSinceMs ?: return
        backgroundSinceMs = null
        val away = clock() - since
        if ((away < 0 || away >= timeoutMs) && hasSession()) _locked.value = true
    }

    companion object {
        /** Two minutes in the background re-locks the app. */
        const val DEFAULT_TIMEOUT_MS = 2 * 60_000L
    }
}
