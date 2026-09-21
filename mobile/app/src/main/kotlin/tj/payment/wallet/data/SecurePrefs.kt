package tj.payment.wallet.data

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import java.security.KeyStore

/**
 * The one way to open a Keystore-encrypted preferences file, shared by the
 * session and the pending-payment store.
 *
 * Creation is guarded. A corrupt Tink keyset or an unusable master key (OS
 * upgrade, restore onto another device, a Keystore fault) used to throw out of
 * a field initialiser — a crash loop at launch with no way out but a reinstall.
 * Now the corrupt file is deleted and recreated; if the master key itself is
 * the problem, that is reset too. The caller learns it happened through
 * [Opened.recovered] and decides what the user must be told.
 */
internal object SecurePrefs {
    class Opened(
        val prefs: SharedPreferences,
        /** The store had to be wiped and recreated: whatever it held is gone. */
        val recovered: Boolean,
        /** False only in the last-resort memory fallback (nothing survives the process). */
        val persistent: Boolean,
    )

    private const val TAG = "SecurePrefs"

    /**
     * One builder for every store. StrongBox (a separate secure element) is
     * requested where the device has one; MasterKey silently falls back to the
     * TEE-backed key where it doesn't, and an existing key is reused as is.
     */
    private fun masterKey(context: Context): MasterKey =
        MasterKey.Builder(context, MasterKey.DEFAULT_MASTER_KEY_ALIAS)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .setRequestStrongBoxBacked(true)
            .build()

    private fun create(context: Context, name: String): SharedPreferences =
        EncryptedSharedPreferences.create(
            context,
            name,
            masterKey(context),
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )

    fun open(context: Context, name: String): Opened {
        val app = context.applicationContext
        try {
            return Opened(create(app, name), recovered = false, persistent = true)
        } catch (first: Exception) {
            Log.w(TAG, "$name unreadable (${first.javaClass.simpleName}); recreating it")
        }

        // 1. The Tink keysets live inside the prefs file itself, so dropping the
        //    file drops the corrupt keyset with it.
        app.deleteSharedPreferences(name)
        try {
            return Opened(create(app, name), recovered = true, persistent = true)
        } catch (second: Exception) {
            Log.w(TAG, "$name still unreadable (${second.javaClass.simpleName}); resetting the master key")
        }

        // 2. The master key is unusable. Deleting it orphans every file it
        //    encrypted — each of them recovers through this same path on its
        //    next open (the session: sign in again; payments: see the notice).
        try {
            KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
                .deleteEntry(MasterKey.DEFAULT_MASTER_KEY_ALIAS)
        } catch (e: Exception) {
            Log.w(TAG, "could not delete the master key", e)
        }
        app.deleteSharedPreferences(name)
        try {
            return Opened(create(app, name), recovered = true, persistent = true)
        } catch (third: Exception) {
            Log.e(TAG, "$name: secure storage unavailable on this device; memory only", third)
        }

        // 3. Last resort: usable and safe beats a crash loop. Nothing persists
        //    across the process — the user signs in each launch, and
        //    PendingPaymentPrefsStore.save() reports false so no payment is
        //    ever sent without a durable key.
        return Opened(MemoryPrefs(), recovered = true, persistent = false)
    }
}

/** In-memory [SharedPreferences] for the secure-storage-unavailable fallback. */
internal class MemoryPrefs : SharedPreferences {
    private val map = HashMap<String, Any?>()

    @Synchronized override fun getAll(): MutableMap<String, *> = HashMap(map)
    @Synchronized override fun getString(key: String, defValue: String?): String? =
        map[key] as? String ?: defValue

    @Suppress("UNCHECKED_CAST")
    @Synchronized override fun getStringSet(key: String, defValues: MutableSet<String>?): MutableSet<String>? =
        (map[key] as? Set<String>)?.toMutableSet() ?: defValues

    @Synchronized override fun getInt(key: String, defValue: Int): Int = map[key] as? Int ?: defValue
    @Synchronized override fun getLong(key: String, defValue: Long): Long = map[key] as? Long ?: defValue
    @Synchronized override fun getFloat(key: String, defValue: Float): Float = map[key] as? Float ?: defValue
    @Synchronized override fun getBoolean(key: String, defValue: Boolean): Boolean =
        map[key] as? Boolean ?: defValue

    @Synchronized override fun contains(key: String): Boolean = map.containsKey(key)
    override fun edit(): SharedPreferences.Editor = MemoryEditor()
    override fun registerOnSharedPreferenceChangeListener(
        listener: SharedPreferences.OnSharedPreferenceChangeListener?,
    ) = Unit

    override fun unregisterOnSharedPreferenceChangeListener(
        listener: SharedPreferences.OnSharedPreferenceChangeListener?,
    ) = Unit

    private inner class MemoryEditor : SharedPreferences.Editor {
        private val puts = LinkedHashMap<String, Any?>()
        private val removes = LinkedHashSet<String>()
        private var clearAll = false

        override fun putString(key: String, value: String?) = also { puts[key] = value }
        override fun putStringSet(key: String, values: MutableSet<String>?) = also { puts[key] = values?.toSet() }
        override fun putInt(key: String, value: Int) = also { puts[key] = value }
        override fun putLong(key: String, value: Long) = also { puts[key] = value }
        override fun putFloat(key: String, value: Float) = also { puts[key] = value }
        override fun putBoolean(key: String, value: Boolean) = also { puts[key] = value }
        override fun remove(key: String) = also { removes += key }
        override fun clear() = also { clearAll = true }

        override fun commit(): Boolean {
            synchronized(this@MemoryPrefs) {
                if (clearAll) map.clear()
                removes.forEach { map.remove(it) }
                puts.forEach { (k, v) -> if (v == null) map.remove(k) else map[k] = v }
            }
            return true
        }

        override fun apply() {
            commit()
        }
    }
}
