package tj.payment.core

import java.security.UnrecoverableKeyException
import javax.crypto.AEADBadTagException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Request-check keys, the app lock, and secure-store recovery. */
class PoliciesTest {

    // --- IntentKey (finding 4: the Request screen) ---

    private data class CheckIntent(val wallet: String, val amountMinor: Long, val description: String?)

    @Test
    fun `an unknown outcome keeps the key for the same intent only`() {
        var n = 0
        val keys = IntentKey<CheckIntent> { "key-${++n}" }
        val a = CheckIntent("w1", 500, null)

        val first = keys.keyFor(a)
        keys.onOutcome(ApiOutcome.Offline(RuntimeException("down")))
        assertEquals("retry of the same check reuses its key", first, keys.keyFor(a))
        keys.onOutcome(ApiOutcome.Failed(ErrorCode.INTERNAL_ERROR, 503, null))
        assertEquals(first, keys.keyFor(a))

        // The amount changed: a different request, so a different key — reusing
        // the old one would be answered 409 idempotency_conflict forever.
        val b = a.copy(amountMinor = 600)
        assertNotEquals(first, keys.keyFor(b))
        assertNotEquals(keys.keyFor(b), keys.keyFor(a.copy(description = "tea")))
        assertNotEquals(keys.keyFor(a.copy(description = "tea")), keys.keyFor(a.copy(wallet = "w2")))
    }

    @Test
    fun `a definitive outcome ends the intent`() {
        var n = 0
        val keys = IntentKey<CheckIntent> { "key-${++n}" }
        val a = CheckIntent("w1", 500, null)
        for (definitive in listOf<ApiOutcome<*>>(
            ApiOutcome.Ok(Unit),
            ApiOutcome.Failed(ErrorCode.KYC_REQUIRED, 403, null),
            ApiOutcome.Failed(ErrorCode.IDEMPOTENCY_CONFLICT, 409, null),
        )) {
            val before = keys.keyFor(a)
            keys.onOutcome(definitive)
            assertNull(keys.heldKey)
            assertNotEquals("after $definitive the same inputs are a new request", before, keys.keyFor(a))
        }
        keys.reset()
        assertNull(keys.heldKey)
    }

    // --- AppLockPolicy (finding 7) ---

    @Test
    fun `the app locks after the timeout in the background, not before`() {
        var now = 0L
        val lock = AppLockPolicy(clock = { now }, timeoutMs = 120_000)
        assertFalse(lock.locked.value)

        lock.onBackground()
        now = 119_999
        lock.onForeground { true }
        assertFalse("a quick app switch does not lock", lock.locked.value)

        lock.onBackground()
        now += 120_000
        lock.onForeground { true }
        assertTrue(lock.locked.value)
        lock.unlock()
        assertFalse(lock.locked.value)
    }

    @Test
    fun `no session means nothing to lock, and a clock running backwards locks`() {
        var now = 1_000_000L
        val lock = AppLockPolicy(clock = { now }, timeoutMs = 120_000)
        lock.onBackground()
        now += 10 * 60_000
        lock.onForeground { false }
        assertFalse(lock.locked.value)

        lock.onBackground()
        now -= 1
        lock.onForeground { true }
        assertTrue(lock.locked.value)
    }

    @Test
    fun `foreground without a background visit is a no-op`() {
        val lock = AppLockPolicy(clock = { 0L })
        lock.onForeground { true }
        assertFalse(lock.locked.value)
        lock.lock()
        assertTrue(lock.locked.value)
    }

    // --- SecureStoreOpener (finding 6) ---

    private class Platform(vararg failures: Exception) {
        val queue = ArrayDeque(failures.toList())
        var wipes = 0
        var keyResets = 0
        val sleeps = mutableListOf<Long>()
        fun create(): String = queue.removeFirstOrNull()?.let { throw it } ?: "prefs"
    }

    private fun opener(p: Platform) = SecureStoreOpener(attempts = 3, sleep = { p.sleeps += it })

    private fun SecureStoreOpener.openWith(p: Platform, failedLaunches: Int = 0) =
        open(create = p::create, wipe = { p.wipes++ }, resetKey = { p.keyResets++ }, failedLaunches = failedLaunches)

    @Test
    fun `a transient fault is retried and destroys nothing`() {
        val p = Platform(java.security.ProviderException("keystore busy"))
        assertEquals(SecureStoreOpener.Result.Opened("prefs", recovered = false), opener(p).openWith(p))
        assertEquals(0, p.wipes)
        assertEquals(0, p.keyResets)
        assertEquals(listOf(100L), p.sleeps)
    }

    @Test
    fun `a persistent transient fault leaves the data alone and reports unavailable`() {
        val p = Platform(*Array(10) { java.security.KeyStoreException("binder died") })
        val result = opener(p).openWith(p)
        assertTrue(result is SecureStoreOpener.Result.Unavailable)
        assertEquals("never wipe on a transient fault", 0, p.wipes)
        assertEquals("never reset a key on a transient fault", 0, p.keyResets)
        assertEquals(listOf(100L, 200L), p.sleeps)
    }

    @Test
    fun `positive evidence of corruption wipes only this store`() {
        val p = Platform(SecurityException("could not decrypt", AEADBadTagException("tag mismatch")))
        assertEquals(SecureStoreOpener.Result.Opened("prefs", recovered = true), opener(p).openWith(p))
        assertEquals(1, p.wipes)
        assertEquals(0, p.keyResets)
        assertTrue("permanent errors are not retried", p.sleeps.isEmpty())
    }

    @Test
    fun `an unusable key is reset only after wiping the file did not help`() {
        val p = Platform(UnrecoverableKeyException("gone"), UnrecoverableKeyException("still gone"))
        assertEquals(SecureStoreOpener.Result.Opened("prefs", recovered = true), opener(p).openWith(p))
        assertEquals(2, p.wipes)
        assertEquals(1, p.keyResets)
    }

    @Test
    fun `the same transient failure across several launches escalates to recreating`() {
        val p = Platform(*Array(3) { java.security.KeyStoreException("still broken") })
        assertEquals(SecureStoreOpener.Result.Opened("prefs", recovered = true), opener(p).openWith(p, failedLaunches = 2))
        assertEquals(1, p.wipes)
    }

    @Test
    fun `fault classification looks through the cause chain`() {
        assertEquals(StorageFault.PERMANENT, StorageFaults.classify(RuntimeException(SecurityException(AEADBadTagException()))))
        assertEquals(StorageFault.TRANSIENT, StorageFaults.classify(IllegalStateException("anything else")))
    }
}
