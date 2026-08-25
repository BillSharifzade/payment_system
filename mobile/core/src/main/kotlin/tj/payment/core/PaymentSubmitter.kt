package tj.payment.core

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/**
 * The client half of the idempotency contract (FRONTEND.md §2.2). A payment the
 * user confirmed is persisted — **with its idempotency key — before the first
 * network attempt**, so no crash, kill, or timeout can lose the key while the
 * server may have posted the money. Retries always reuse the same key; the
 * server then charges at most once no matter how many times we ask.
 */
@Serializable
data class PendingPayment(
    @SerialName("idempotency_key") val idempotencyKey: String,
    @SerialName("from_account") val fromAccount: String,
    @SerialName("to_account") val toAccount: String,
    @SerialName("amount_minor") val amountMinor: Long,
    val currency: String,
    /** What the user saw on the confirm screen (phone or verified name) — shown
     * again if the app restarts with this payment unresolved. */
    @SerialName("recipient_label") val recipientLabel: String,
)

/** Durable storage for at most one in-flight payment. */
interface PendingPaymentStore {
    fun load(): PendingPayment?
    fun save(payment: PendingPayment)
    fun clear()
}

/** The outcome of one submission attempt. */
sealed interface SubmitResult {
    /** Money moved (or had already moved — a replayed retry). Key cleared. */
    data class Posted(val transactionId: String, val alreadyPosted: Boolean) : SubmitResult

    /**
     * The server definitively refused (insufficient funds, limit, bad recipient…).
     * The key is cleared: this exact payment will never post, so a corrected
     * attempt is a NEW payment with a new key.
     */
    data class Rejected(val code: ErrorCode, val serverMessage: String?) : SubmitResult

    /**
     * Unknown outcome — offline, a 5xx, or rate-limited. The server may or may
     * not have posted. The pending payment (and key) is KEPT; the only safe next
     * step is [PaymentSubmitter.retryPending] with the same key.
     */
    data class Unsettled(val offline: Boolean) : SubmitResult
}

/**
 * Drives a payment from "user confirmed" to a settled outcome. Pure logic —
 * networking and persistence are injected, so this state machine unit-tests on
 * the JVM.
 */
class PaymentSubmitter(
    private val store: PendingPaymentStore,
    private val newKey: () -> String,
    private val transfer: suspend (PendingPayment) -> ApiOutcome<PostResponse>,
) {
    /** The unresolved payment from a previous attempt/app run, if any. */
    fun pending(): PendingPayment? = store.load()

    /**
     * Persist and submit a payment the user just confirmed. Refuses (returns
     * null) while an earlier payment is still unsettled — that one must be
     * retried or resolved first, or two keys could race for one intent.
     */
    suspend fun submitNew(
        fromAccount: String,
        toAccount: String,
        amountMinor: Long,
        currency: String,
        recipientLabel: String,
    ): SubmitResult? {
        if (store.load() != null) return null
        val payment = PendingPayment(
            idempotencyKey = newKey(),
            fromAccount = fromAccount,
            toAccount = toAccount,
            amountMinor = amountMinor,
            currency = currency,
            recipientLabel = recipientLabel,
        )
        store.save(payment) // durable BEFORE the first attempt
        return attempt(payment)
    }

    /** Retry the stored payment with its original key. Null if nothing pending. */
    suspend fun retryPending(): SubmitResult? = store.load()?.let { attempt(it) }

    /**
     * Drop an unsettled payment WITHOUT knowing its outcome. Only for an explicit
     * user decision ("discard this payment") — never called by program logic.
     */
    fun abandonPending() = store.clear()

    private suspend fun attempt(payment: PendingPayment): SubmitResult =
        when (val outcome = transfer(payment)) {
            is ApiOutcome.Ok -> {
                store.clear()
                SubmitResult.Posted(
                    transactionId = outcome.value.transactionId,
                    alreadyPosted = outcome.value.status == "already_posted",
                )
            }
            is ApiOutcome.Failed ->
                if (outcome.code == ErrorCode.RATE_LIMITED || outcome.httpStatus >= 500) {
                    // The server didn't rule; it may yet post on retry.
                    SubmitResult.Unsettled(offline = false)
                } else {
                    store.clear()
                    SubmitResult.Rejected(outcome.code, outcome.serverMessage)
                }
            is ApiOutcome.Offline -> SubmitResult.Unsettled(offline = true)
        }
}
