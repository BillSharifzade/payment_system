package tj.payment.wallet.data

import android.content.Context
import android.content.SharedPreferences
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import kotlinx.serialization.json.Json
import tj.payment.core.PendingPayment
import tj.payment.core.PendingPaymentStore

/**
 * Durable home of the (at most one) in-flight payment, Keystore-encrypted like
 * the session. Written BEFORE the first network attempt and cleared only on a
 * definitive outcome — this file is why a killed app can't double-charge or
 * forget an unsettled payment.
 */
class PendingPaymentPrefsStore(context: Context) : PendingPaymentStore {

    private val json = Json { ignoreUnknownKeys = true }

    private val prefs: SharedPreferences = run {
        val masterKey = MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build()
        EncryptedSharedPreferences.create(
            context,
            "payments.secure",
            masterKey,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    override fun load(): PendingPayment? {
        val raw = prefs.getString(KEY, null) ?: return null
        return try {
            json.decodeFromString(PendingPayment.serializer(), raw)
        } catch (_: Exception) {
            // A corrupt record is unreadable and unretriable; drop it rather
            // than brick the send flow forever.
            prefs.edit().remove(KEY).apply()
            null
        }
    }

    override fun save(payment: PendingPayment) {
        prefs.edit()
            .putString(KEY, json.encodeToString(PendingPayment.serializer(), payment))
            .commit() // synchronous on purpose: must hit disk before the request goes out
    }

    override fun clear() {
        prefs.edit().remove(KEY).apply()
    }

    private companion object {
        const val KEY = "pending_payment"
    }
}
