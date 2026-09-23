package tj.payment.core

import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
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
    /**
     * Set when this payment settles a merchant check (pay-by-QR): the transfer
     * then goes through POST /v1/checks/{checkId}/pay instead of /v1/transfers.
     * Same key rules — the server uses the key as the transaction id either way.
     */
    @SerialName("check_id") val checkId: String? = null,
)

/** Durable storage for at most one in-flight payment. */
interface PendingPaymentStore {
    fun load(): PendingPayment?

    /**
     * Write synchronously and report whether the record reached durable storage.
     * `false` means the key is NOT safe on disk — the submitter then refuses to
     * go to the network (persist-before-network is the invariant, not a hope).
     */
    fun save(payment: PendingPayment): Boolean

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
     * Unknown outcome — offline, a 5xx / `retry_later`, rate-limited, or a 2xx
     * whose body we could not read. The server may or may not have posted. The
     * pending payment (and key) is KEPT; the only safe next step is
     * [PaymentSubmitter.retryPending] with the same key.
     */
    data class Unsettled(val offline: Boolean) : SubmitResult

    /**
     * Nothing left the device: the pending record could not be persisted, so the
     * network was never attempted. Nothing to retry — the user may simply try
     * again (a fresh key will be minted because nothing is stored).
     */
    data object NotStarted : SubmitResult
}

/** What happened when the user asked to drop an unsettled payment. */
sealed interface DiscardResult {
    /** The statement proved it had posted after all; resolved as sent, key cleared. */
    data class WasPosted(val transactionId: String) : DiscardResult

    /** Not in the statement; the record is gone. */
    data object Discarded : DiscardResult

    /** Could not read the statement, so the record was KEPT — never drop blind. */
    data class CouldNotVerify(val offline: Boolean) : DiscardResult
}

/**
 * Drives a payment from "user confirmed" to a settled outcome. Pure logic —
 * networking and persistence are injected, so this state machine unit-tests on
 * the JVM.
 *
 * @param findPosted Looks the payment up in its source wallet's statement by
 *   idempotency key (the backend uses the key as the transaction id).
 *   `Ok(true)` = it posted, `Ok(false)` = not there, `Failed`/`Offline` = could
 *   not tell. Used as a second opinion before any path that would forget a key.
 * @param io Where the (synchronous, disk-hitting) store calls run. The default
 *   keeps them off the main thread; tests may pass an immediate dispatcher.
 */
class PaymentSubmitter(
    private val store: PendingPaymentStore,
    private val newKey: () -> String,
    private val transfer: suspend (PendingPayment) -> ApiOutcome<PostResponse>,
    private val findPosted: suspend (PendingPayment) -> ApiOutcome<Boolean> = { ApiOutcome.Ok(false) },
    private val io: CoroutineDispatcher = Dispatchers.IO,
) {
    /** The unresolved payment from a previous attempt/app run, if any. */
    suspend fun pending(): PendingPayment? = load()

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
        checkId: String? = null,
    ): SubmitResult? {
        if (load() != null) return null
        val payment = PendingPayment(
            idempotencyKey = newKey(),
            fromAccount = fromAccount,
            toAccount = toAccount,
            amountMinor = amountMinor,
            currency = currency,
            recipientLabel = recipientLabel,
            checkId = checkId,
        )
        // Durable BEFORE the first attempt — and if it isn't, the attempt never
        // happens: a key that exists only in memory can be lost mid-flight.
        if (!save(payment)) return SubmitResult.NotStarted
        return attempt(payment, isRetry = false)
    }

    /** Retry the stored payment with its original key. Null if nothing pending. */
    suspend fun retryPending(): SubmitResult? = load()?.let { attempt(it, isRetry = true) }

    /**
     * Drop an unsettled payment on an explicit user decision — but only after
     * checking the statement: if the key is there, the money moved and we say
     * so instead. Never called by program logic.
     */
    suspend fun discardPending(): DiscardResult? {
        val payment = load() ?: return null
        return when (val seen = findPosted(payment)) {
            is ApiOutcome.Ok -> {
                clear()
                if (seen.value) DiscardResult.WasPosted(payment.idempotencyKey) else DiscardResult.Discarded
            }
            is ApiOutcome.Failed -> DiscardResult.CouldNotVerify(offline = false)
            is ApiOutcome.Offline -> DiscardResult.CouldNotVerify(offline = true)
        }
    }

    // The store is synchronous by contract (persist-before-network must be
    // observable); these keep its disk I/O off the caller's (UI) thread.
    private suspend fun load(): PendingPayment? = withContext(io) { store.load() }
    private suspend fun save(payment: PendingPayment): Boolean = withContext(io) { store.save(payment) }
    private suspend fun clear() = withContext(io) { store.clear() }

    private suspend fun attempt(payment: PendingPayment, isRetry: Boolean): SubmitResult =
        when (val outcome = transfer(payment)) {
            is ApiOutcome.Ok -> {
                clear()
                SubmitResult.Posted(
                    transactionId = outcome.value.transactionId,
                    alreadyPosted = outcome.value.status == "already_posted",
                )
            }
            is ApiOutcome.Failed -> when {
                // 2xx-unreadable, 5xx, retry_later, 429: the server didn't rule
                // (or ruled and we lost the answer). It may yet post on retry.
                outcome.undetermined -> SubmitResult.Unsettled(offline = false)
                // On a RETRY these are answered by the auth/KYC gate, before the
                // idempotency replay — so they say nothing about whether the
                // FIRST attempt posted. Keep the key.
                isRetry && outcome.code in GATE_CODES -> SubmitResult.Unsettled(offline = false)
                // A definitive refusal of a retry: trust it only after the
                // statement agrees the key never posted.
                isRetry -> reconcileRejection(payment, outcome)
                else -> {
                    clear()
                    SubmitResult.Rejected(outcome.code, outcome.serverMessage)
                }
            }
            is ApiOutcome.Offline -> SubmitResult.Unsettled(offline = true)
        }

    private suspend fun reconcileRejection(payment: PendingPayment, refusal: ApiOutcome.Failed): SubmitResult =
        when (val seen = findPosted(payment)) {
            is ApiOutcome.Ok -> {
                clear()
                if (seen.value) {
                    SubmitResult.Posted(transactionId = payment.idempotencyKey, alreadyPosted = true)
                } else {
                    SubmitResult.Rejected(refusal.code, refusal.serverMessage)
                }
            }
            // Can't corroborate: keep the key rather than forget a payment that
            // may have posted. The user can retry (or discard, which re-checks).
            is ApiOutcome.Failed -> SubmitResult.Unsettled(offline = false)
            is ApiOutcome.Offline -> SubmitResult.Unsettled(offline = true)
        }

    private companion object {
        val GATE_CODES = setOf(ErrorCode.UNAUTHORIZED, ErrorCode.FORBIDDEN, ErrorCode.KYC_REQUIRED)
    }
}
