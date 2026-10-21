package tj.payment.wallet.data

import android.content.Context
import tj.payment.core.KeyValuePendingPaymentStore
import tj.payment.core.LocalStorageException
import tj.payment.core.PendingPayment
import tj.payment.core.PendingPaymentStore

/**
 * Durable home of in-flight payments — at most one per user — Keystore-encrypted
 * in its own file with its own master key (see [SecureStore]). Written BEFORE
 * the first network attempt and cleared only on a definitive outcome: this file
 * is why a killed app can't double-charge or forget an unsettled payment.
 *
 * All the logic (per-user slots, adopting a pre-upgrade record, dropping a
 * corrupt record) is [KeyValuePendingPaymentStore] in :core, unit-tested on the
 * JVM; this class only supplies the encrypted key-value port.
 *
 * If the store had to be recreated, an unsettled payment may have been in it
 * and is now gone: a one-time notice is recorded for Home so the user checks
 * History before sending again. If secure storage is unavailable this launch,
 * every read and write throws [LocalStorageException] — the submitter then
 * refuses to start any payment (nothing is ever sent without a durable key).
 */
class PendingPaymentPrefsStore(context: Context) : PendingPaymentStore {

    private val store = SecureStore(
        context,
        fileName = "payments.v2.secure",
        keyAlias = "tj.payment.wallet.mk.payments",
        legacyFileName = "payments.secure",
    )
    private val records = KeyValuePendingPaymentStore(PrefsKeyValue(store))

    init {
        if (store.opened.recovered) {
            try {
                records.recordUnreadableRecord()
            } catch (_: LocalStorageException) {
                // Nothing more we can do; the store is unusable anyway.
            }
        }
    }

    /**
     * One-time flag: true if an earlier payment record was lost to corruption
     * and the user has not yet been told. Cleared by this call.
     */
    fun consumeUnreadableRecordNotice(): Boolean = try {
        records.consumeUnreadableRecordNotice()
    } catch (_: LocalStorageException) {
        false
    }

    override fun load(userId: String): PendingPayment? = records.load(userId)

    override fun save(payment: PendingPayment): Boolean = records.save(payment)

    override fun clear(userId: String, idempotencyKey: String) = records.clear(userId, idempotencyKey)
}
