package tj.payment.wallet.data

import android.content.Context
import kotlinx.serialization.json.Json
import tj.payment.core.PendingPayment
import tj.payment.core.PendingPaymentStore

/**
 * Durable home of the (at most one) in-flight payment, Keystore-encrypted like
 * the session. Written BEFORE the first network attempt and cleared only on a
 * definitive outcome — this file is why a killed app can't double-charge or
 * forget an unsettled payment.
 *
 * Opening is guarded (see [SecurePrefs]). If the store had to be recreated, an
 * unsettled payment may have been in it and is now unreadable: a one-time
 * notice is recorded for Home so the user checks History before sending again.
 */
class PendingPaymentPrefsStore(context: Context) : PendingPaymentStore {

    private val json = Json { ignoreUnknownKeys = true }

    private val opened = SecurePrefs.open(context, "payments.secure")
    private val prefs get() = opened.prefs

    init {
        if (opened.recovered) recordUnreadableRecord()
    }

    /**
     * One-time flag: true if an earlier payment record was lost to corruption
     * and the user has not yet been told. Cleared by this call.
     */
    fun consumeUnreadableRecordNotice(): Boolean {
        if (!prefs.getBoolean(KEY_UNREADABLE_NOTICE, false)) return false
        prefs.edit().remove(KEY_UNREADABLE_NOTICE).apply()
        return true
    }

    override fun load(): PendingPayment? {
        val raw = prefs.getString(KEY, null) ?: return null
        return try {
            json.decodeFromString(PendingPayment.serializer(), raw)
        } catch (_: Exception) {
            // A corrupt record is unreadable and unretriable; drop it rather
            // than brick the send flow forever — but tell the user, once.
            prefs.edit().remove(KEY).apply()
            recordUnreadableRecord()
            null
        }
    }

    /**
     * Synchronous on purpose: the record must hit disk before the request goes
     * out, and the caller must know if it didn't. A memory-only fallback store
     * is never "saved" — the submitter then refuses to send.
     */
    override fun save(payment: PendingPayment): Boolean {
        if (!opened.persistent) return false
        return prefs.edit()
            .putString(KEY, json.encodeToString(PendingPayment.serializer(), payment))
            .commit()
    }

    override fun clear() {
        prefs.edit().remove(KEY).apply()
    }

    private fun recordUnreadableRecord() {
        prefs.edit().putBoolean(KEY_UNREADABLE_NOTICE, true).commit()
    }

    private companion object {
        const val KEY = "pending_payment"
        const val KEY_UNREADABLE_NOTICE = "unreadable_record_notice"
    }
}
