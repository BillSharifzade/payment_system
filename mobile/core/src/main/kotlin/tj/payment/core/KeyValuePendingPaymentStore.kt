package tj.payment.core

import kotlinx.serialization.json.Json

/**
 * The minimal synchronous key-value port a durable store needs. The Android
 * app implements it over Keystore-backed EncryptedSharedPreferences; tests use
 * a map. Implementations throw [LocalStorageException] when the underlying
 * storage faults — never return null/false to mean "it broke".
 */
interface SecureKeyValue {
    fun get(key: String): String?

    /** Durable (synchronous commit). False if the value is not safely on disk. */
    fun put(key: String, value: String): Boolean

    fun remove(key: String): Boolean

    fun getFlag(key: String): Boolean

    fun putFlag(key: String, value: Boolean): Boolean
}

/**
 * [PendingPaymentStore] over a [SecureKeyValue]: one slot per user
 * (`pending_payment.<userId>`), so user A's unsettled payment is invisible to —
 * and never blocks — user B on a shared phone.
 *
 * Migration: app versions before per-user binding kept ONE record under
 * `pending_payment` with no owner. Such a legacy record is adopted by the first
 * user who loads after the upgrade (in practice the user who was signed in
 * when it was written). If that guess is wrong, the record's key belongs to the
 * other user's accounts: voiding it from the wrong account answers a coded 404,
 * which [PaymentSubmitter] treats as "not yours, nothing of yours sent" — so a
 * misattributed record can always be cleared and never locks anyone out.
 *
 * A record that cannot be decoded is dropped (it is unretriable) and a one-time
 * notice is raised so the user checks History before paying again.
 */
class KeyValuePendingPaymentStore(private val kv: SecureKeyValue) : PendingPaymentStore {

    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }

    override fun load(userId: String): PendingPayment? {
        require(userId.isNotBlank()) { "a pending payment always has an owner" }
        kv.get(keyFor(userId))?.let { raw ->
            val record = decodeOrDrop(keyFor(userId), raw) ?: return null
            // Defensive: a slot only ever holds its owner's record.
            return record.takeIf { it.userId == userId }
        }
        val legacy = kv.get(LEGACY_KEY) ?: return null
        val record = decodeOrDrop(LEGACY_KEY, legacy) ?: return null
        if (record.userId.isNotEmpty() && record.userId != userId) return null
        val adopted = record.copy(userId = userId)
        // Move it into the owner's slot; only then drop the legacy copy, so a
        // crash in between leaves (at worst) the record in both places.
        if (kv.put(keyFor(userId), encode(adopted))) kv.remove(LEGACY_KEY)
        return adopted
    }

    override fun save(payment: PendingPayment): Boolean {
        require(payment.userId.isNotBlank()) { "a pending payment always has an owner" }
        return kv.put(keyFor(payment.userId), encode(payment))
    }

    override fun clear(userId: String, idempotencyKey: String) {
        val key = keyFor(userId)
        kv.get(key)?.let { raw ->
            val record = decodeOrDrop(key, raw)
            // Never clear a NEWER record that replaced the one being settled.
            if (record != null && record.idempotencyKey == idempotencyKey) kv.remove(key)
        }
        // A legacy copy of the same payment (an adoption interrupted between
        // its two writes) must not be adopted again once the payment settled.
        kv.get(LEGACY_KEY)?.let { raw ->
            val legacy = decodeOrDrop(LEGACY_KEY, raw)
            if (legacy != null && legacy.idempotencyKey == idempotencyKey) kv.remove(LEGACY_KEY)
        }
    }

    /**
     * One-time flag: true if a payment record was lost to corruption (or the
     * store had to be recreated) and nobody has been told yet. Cleared by this call.
     */
    fun consumeUnreadableRecordNotice(): Boolean {
        if (!kv.getFlag(UNREADABLE_NOTICE_KEY)) return false
        kv.remove(UNREADABLE_NOTICE_KEY)
        return true
    }

    /** Raise the one-time notice (the store was recreated and may have held a record). */
    fun recordUnreadableRecord() {
        kv.putFlag(UNREADABLE_NOTICE_KEY, true)
    }

    private fun encode(payment: PendingPayment): String = json.encodeToString(PendingPayment.serializer(), payment)

    private fun decodeOrDrop(key: String, raw: String): PendingPayment? = try {
        json.decodeFromString(PendingPayment.serializer(), raw)
    } catch (_: Exception) {
        // Unreadable and unretriable: drop it rather than brick payments
        // forever — but tell the user, once.
        kv.remove(key)
        recordUnreadableRecord()
        null
    }

    companion object {
        const val LEGACY_KEY = "pending_payment"
        const val UNREADABLE_NOTICE_KEY = "unreadable_record_notice"

        fun keyFor(userId: String): String = "$LEGACY_KEY.$userId"
    }
}
