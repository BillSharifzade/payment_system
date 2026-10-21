package tj.payment.wallet.data

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import java.io.File
import java.io.IOException
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.security.ProviderException
import tj.payment.core.LocalStorageException
import tj.payment.core.SecureKeyValue
import tj.payment.core.SecureStoreOpener
import tj.payment.core.StorageFault
import tj.payment.core.StorageFaults

/**
 * One Keystore-encrypted preferences file with **its own master key**. The
 * session and the pending-payment store used to share the library's default
 * master key, and recovery deleted that shared key on any exception — a
 * transient Keystore hiccup while opening one store destroyed the other
 * (losing an unsettled payment's key). Now:
 *
 *  - each store has its own key alias, so resetting one can never affect the other;
 *  - opening follows [SecureStoreOpener]: transient faults are retried and,
 *    if they persist, the store runs memory-only for this process WITHOUT
 *    deleting anything; only positive evidence of corruption (or the same
 *    failure on several consecutive launches) recreates this store alone;
 *  - a pre-hardening file (encrypted with the shared default key) is migrated
 *    into the new file once, then deleted; the default key itself is never
 *    deleted (some other file might still need it).
 *
 * The caller learns what happened through [Opened.recovered] (the store had
 * to be recreated: whatever it held is gone) and [Opened.persistent] (false =
 * memory-only fallback for this process).
 */
internal class SecureStore(
    context: Context,
    private val fileName: String,
    private val keyAlias: String,
    /** The pre-hardening file to migrate from (encrypted with the shared default key), if any. */
    private val legacyFileName: String?,
) {
    class Opened(
        val prefs: SharedPreferences,
        /** The store had to be wiped and recreated: whatever it held is gone. */
        val recovered: Boolean,
        /** False only in the memory fallback (nothing survives the process; nothing was deleted). */
        val persistent: Boolean,
    )

    private val app = context.applicationContext

    /** Opened once per process, at construction (callers construct off the main thread). */
    val opened: Opened = open()

    private fun open(): Opened {
        val failedLaunches = health.getInt(fileName, 0)
        val giveUpOnLegacy = failedLaunches + 1 >= ESCALATE_AFTER_FAILED_LAUNCHES
        var legacyLost = false
        val result = SecureStoreOpener(escalateAfterFailedLaunches = ESCALATE_AFTER_FAILED_LAUNCHES).open(
            create = {
                val prefs = create(fileName, keyAlias)
                if (migrateLegacy(prefs, giveUpOnLegacy)) legacyLost = true
                prefs
            },
            wipe = { app.deleteSharedPreferences(fileName) },
            resetKey = { deleteKey(keyAlias) },
            failedLaunches = failedLaunches,
        )
        return when (result) {
            is SecureStoreOpener.Result.Opened -> {
                health.edit().remove(fileName).commit()
                if (result.recovered) Log.w(TAG, "$fileName was unreadable and has been recreated")
                Opened(result.value, recovered = result.recovered || legacyLost, persistent = true)
            }
            is SecureStoreOpener.Result.Unavailable -> {
                health.edit().putInt(fileName, failedLaunches + 1).commit()
                Log.e(TAG, "$fileName: secure storage unavailable this launch; memory only, nothing deleted", result.cause)
                // Last resort: usable and safe beats a crash loop. Nothing
                // persists across the process; PendingPaymentPrefsStore refuses
                // to save (or even read) so no payment is ever sent without a
                // durable key.
                Opened(MemoryPrefs(), recovered = false, persistent = false)
            }
        }
    }

    /**
     * Move the pre-hardening file's entries into [into] (once), then delete it.
     * Returns true if the legacy data had to be given up (unreadable).
     * A transient failure throws, so the whole open is retried and nothing is lost.
     */
    private fun migrateLegacy(into: SharedPreferences, giveUp: Boolean): Boolean {
        val legacy = legacyFileName ?: return false
        if (!File(app.dataDir, "shared_prefs/$legacy.xml").exists()) return false
        if (into.all.isNotEmpty()) {
            // Already migrated (the delete below didn't happen last time).
            app.deleteSharedPreferences(legacy)
            return false
        }
        val old = try {
            create(legacy, MasterKey.DEFAULT_MASTER_KEY_ALIAS)
        } catch (e: Exception) {
            if (!giveUp && StorageFaults.classify(e) == StorageFault.TRANSIENT) throw e
            Log.w(TAG, "$legacy unreadable (${e.javaClass.simpleName}); giving it up")
            app.deleteSharedPreferences(legacy)
            return true
        }
        val editor = into.edit()
        for ((key, value) in old.all) {
            when (value) {
                is String -> editor.putString(key, value)
                is Boolean -> editor.putBoolean(key, value)
                is Int -> editor.putInt(key, value)
                is Long -> editor.putLong(key, value)
                is Float -> editor.putFloat(key, value)
                is Set<*> -> editor.putStringSet(key, value.filterIsInstance<String>().toSet())
            }
        }
        if (!editor.commit()) throw IOException("could not write the migrated $legacy entries")
        app.deleteSharedPreferences(legacy)
        return false
    }

    /**
     * StrongBox (a separate secure element) is requested where the device has
     * one; MasterKey silently falls back to the TEE-backed key where it doesn't,
     * and an existing key is reused as is.
     */
    private fun create(name: String, alias: String): SharedPreferences {
        val masterKey = MasterKey.Builder(app, alias)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .setRequestStrongBoxBacked(true)
            .build()
        return EncryptedSharedPreferences.create(
            app,
            name,
            masterKey,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    /** Only ever this store's own alias — never the shared default. */
    private fun deleteKey(alias: String) {
        check(alias != MasterKey.DEFAULT_MASTER_KEY_ALIAS) { "the shared default master key is never deleted" }
        try {
            KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(alias)
        } catch (e: Exception) {
            Log.w(TAG, "could not delete $alias", e)
        }
    }

    /** Plain prefs (non-sensitive): consecutive launches whose open failed, per store. */
    private val health: SharedPreferences
        get() = app.getSharedPreferences(HEALTH_FILE, Context.MODE_PRIVATE)

    private companion object {
        const val TAG = "SecureStore"
        const val HEALTH_FILE = "secure_store_health"
        const val ESCALATE_AFTER_FAILED_LAUNCHES = 3
    }
}

/**
 * Runs [block] against encrypted prefs, turning a Keystore/crypto failure into
 * the typed [LocalStorageException] (EncryptedSharedPreferences throws
 * SecurityException("Could not decrypt value…") and friends).
 */
internal inline fun <T> secureIo(block: () -> T): T = try {
    block()
} catch (e: SecurityException) {
    throw LocalStorageException("secure storage unavailable", e)
} catch (e: GeneralSecurityException) {
    throw LocalStorageException("secure storage unavailable", e)
} catch (e: ProviderException) {
    throw LocalStorageException("secure storage unavailable", e)
}

/**
 * [SecureKeyValue] over a [SecureStore] for the pending-payment store. In the
 * memory fallback it refuses reads as well as writes: a payment record may be
 * sitting unreadable on disk, so "nothing stored" would be a lie.
 */
internal class PrefsKeyValue(private val store: SecureStore) : SecureKeyValue {
    private fun prefs(): SharedPreferences {
        val opened = store.opened
        if (!opened.persistent) throw LocalStorageException("secure storage unavailable this launch")
        return opened.prefs
    }

    override fun get(key: String): String? = secureIo { prefs().getString(key, null) }

    override fun put(key: String, value: String): Boolean = secureIo { prefs().edit().putString(key, value).commit() }

    override fun remove(key: String): Boolean = secureIo { prefs().edit().remove(key).commit() }

    override fun getFlag(key: String): Boolean = secureIo { prefs().getBoolean(key, false) }

    override fun putFlag(key: String, value: Boolean): Boolean = secureIo { prefs().edit().putBoolean(key, value).commit() }
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
