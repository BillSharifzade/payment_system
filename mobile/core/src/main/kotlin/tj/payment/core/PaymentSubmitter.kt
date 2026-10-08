package tj.payment.core

import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** What a persisted payment does when it is (re)submitted. */
@Serializable
enum class PaymentKind {
    /** POST /v1/transfers — money to another user. */
    @SerialName("transfer")
    TRANSFER,

    /** POST /v1/checks/{checkId}/pay — settle a merchant's check (pay-by-QR). */
    @SerialName("check")
    CHECK,

    /** POST /v1/fx — convert between the user's own wallets. */
    @SerialName("fx")
    FX,
}

/**
 * The client half of the idempotency contract (FRONTEND.md §2.2). A payment the
 * user confirmed is persisted — **with its idempotency key — before the first
 * network attempt**, so no crash, kill, or timeout can lose the key while the
 * server may have posted the money. Retries always reuse the same key; the
 * server then charges at most once no matter how many times we ask.
 *
 * Every record belongs to exactly one user ([userId]). Only that user ever sees,
 * retries or discards it, and it never blocks anyone else — a shared phone with
 * an unsettled payment from user A lets user B pay normally.
 */
@Serializable
data class PendingPayment(
    @SerialName("idempotency_key") val idempotencyKey: String,
    @SerialName("from_account") val fromAccount: String,
    @SerialName("to_account") val toAccount: String,
    @SerialName("amount_minor") val amountMinor: Long,
    /** The debited currency (for FX: the source wallet's). */
    val currency: String,
    /**
     * What the user saw on the confirm screen (phone, verified name, merchant;
     * for FX the target currency) — shown again if the payment is unresolved.
     */
    @SerialName("recipient_label") val recipientLabel: String,
    /** The merchant check this payment settles ([PaymentKind.CHECK] only). */
    @SerialName("check_id") val checkId: String? = null,
    /**
     * The owner. Empty only on a record written by an app version that predated
     * per-user binding; the store hands such a record to the first user who
     * loads it (see [KeyValuePendingPaymentStore]).
     */
    @SerialName("user_id") val userId: String = "",
    val kind: PaymentKind = if (checkId != null) PaymentKind.CHECK else PaymentKind.TRANSFER,
    /**
     * Base64 device signature over [authorizationPayload], made with the
     * Keystore key that only a successful strong device authentication unlocks.
     * Null only on legacy records. Kept with the record so every retry carries
     * the original approval (and so server-side verification can be added
     * without changing the record).
     */
    val authorization: String? = null,
    @SerialName("created_at_ms") val createdAtMs: Long = 0,
)

/**
 * The exact bytes the device owner approves for a payment: every field that
 * decides where money goes and how much, plus the key (so an approval can never
 * be replayed for another payment). Length-prefixed, so no field value can be
 * confused with a separator. Versioned for a future server-side verifier.
 */
fun PendingPayment.authorizationPayload(): ByteArray =
    listOf(
        "tj.payment.authorize.v1",
        userId,
        kind.name.lowercase(),
        idempotencyKey,
        fromAccount,
        toAccount,
        amountMinor.toString(),
        currency,
        checkId.orEmpty(),
    ).joinToString(separator = "") { "${it.length}:$it;" }.toByteArray(Charsets.UTF_8)

/**
 * Strong device authentication for a money move. The implementation shows the
 * platform prompt (fingerprint/face of the strongest class, or the screen-lock
 * PIN/pattern) and, on success, signs [authorizationPayload] with a Keystore key
 * created with user authentication required — so a "yes" cannot be faked by
 * skipping or spoofing the prompt UI; only the Keystore can produce it.
 */
fun interface PaymentAuthorizer {
    suspend fun authorize(payment: PendingPayment): Authorization
}

sealed interface Authorization {
    /** The device owner approved exactly this payment. */
    data class Granted(val signature: String) : Authorization

    /** The user dismissed the prompt. */
    data object Cancelled : Authorization

    data class Denied(val reason: AuthDenial) : Authorization
}

/** Why a money move could not be authorized on this device. */
enum class AuthDenial {
    /** No strong biometric and no screen lock set up: nothing to bind the payment to. */
    NOT_ENROLLED,

    /** No usable sensor / Keystore on this device right now. */
    UNAVAILABLE,

    /** The platform requires a security update before it will authenticate. */
    SECURITY_UPDATE_REQUIRED,

    /** Too many failed attempts; authentication is locked for now. */
    LOCKED_OUT,

    /** The app is not on screen, so there is nowhere to show the prompt. */
    NO_SCREEN,

    /** The prompt or the Keystore signing failed. */
    FAILED,
}

/** Durable storage for at most one in-flight payment **per user**. */
interface PendingPaymentStore {
    /**
     * [userId]'s unsettled payment, or null when there is none.
     *
     * @throws LocalStorageException when the store cannot be read right now —
     *   which callers must never treat as "nothing pending".
     */
    fun load(userId: String): PendingPayment?

    /**
     * Write synchronously (keyed by [PendingPayment.userId]) and report whether
     * the record reached durable storage. `false` means the key is NOT safe on
     * disk — the submitter then refuses to go to the network
     * (persist-before-network is the invariant, not a hope).
     */
    fun save(payment: PendingPayment): Boolean

    /** Remove [userId]'s record — only if it is still the one with [idempotencyKey]. */
    fun clear(userId: String, idempotencyKey: String)
}

/** What a money-moving endpoint answered, normalized across transfer / check / FX. */
data class Settlement(
    val transactionId: String,
    /** The server replayed an earlier success for this key. */
    val alreadyPosted: Boolean,
    /** FX only: the exact debit/credit, when the answer carried them. */
    val fx: FxResponse? = null,
)

/** The outcome of one submission attempt. */
sealed interface SubmitResult {
    /** Money moved (or had already moved — a replayed retry). Key cleared. */
    data class Posted(
        val transactionId: String,
        val alreadyPosted: Boolean,
        val fx: FxResponse? = null,
    ) : SubmitResult

    /**
     * The server definitively refused, and the key can never post (the refusal
     * was a first answer, or a retry's refusal was confirmed by voiding the
     * key). Cleared: a corrected attempt is a NEW payment with a new key.
     */
    data class Rejected(val code: ErrorCode, val serverMessage: String?) : SubmitResult

    /**
     * Unknown outcome — offline, a 5xx / `retry_later`, rate-limited, a 2xx
     * whose body we could not read, or a refusal we could not confirm. The
     * server may or may not have posted. The pending payment (and key) is KEPT;
     * the safe next steps are [PaymentSubmitter.retryPending] (same key) or
     * [PaymentSubmitter.discardPending] (void the key).
     */
    data class Unsettled(val offline: Boolean) : SubmitResult

    /**
     * Nothing left the device: no signed-in user, or the pending record could
     * not be persisted / the store could not be read. Nothing to retry.
     */
    data object NotStarted : SubmitResult

    /**
     * The device owner did not approve ([denial] null = they dismissed the
     * prompt). Nothing was stored, nothing was sent.
     */
    data class NotAuthorized(val denial: AuthDenial?) : SubmitResult

    /**
     * This user already has an unsettled payment; it must be finished or
     * discarded first (one key per intent — two keys must never race).
     */
    data class Blocked(val pending: PendingPayment) : SubmitResult
}

/** What happened when the user asked to drop an unsettled payment. */
sealed interface DiscardResult {
    /** Voiding found it had posted after all; resolved as sent, key cleared. */
    data class WasPosted(val transactionId: String) : DiscardResult

    /** The key is voided server-side — it can never post. The record is gone. */
    data object Discarded : DiscardResult

    /** The void did not get a definitive answer, so the record was KEPT. */
    data class CouldNotVerify(val offline: Boolean) : DiscardResult
}

/** A read-only status check of the pending payment (GET /v1/transactions/{key}). */
sealed interface PendingCheck {
    val payment: PendingPayment

    /** It posted. Record cleared. */
    data class Posted(override val payment: PendingPayment, val transactionId: String) : PendingCheck

    /** The key was voided: it can never post. Record cleared. */
    data class Voided(override val payment: PendingPayment) : PendingCheck

    /** Not posted (yet) — it may still be in flight. Record kept. */
    data class NotPosted(override val payment: PendingPayment) : PendingCheck

    /** Could not tell. Record kept. */
    data class Unknown(override val payment: PendingPayment, val offline: Boolean) : PendingCheck
}

/**
 * Drives a payment from "user confirmed" to a settled outcome. Pure logic —
 * networking, persistence and device authentication are injected, so this state
 * machine unit-tests on the JVM.
 *
 * Invariants:
 *  - nothing goes to the network unless the device owner approved this exact
 *    payment ([authorizer]) AND the record (with that approval and its key) is
 *    durably stored;
 *  - a key is forgotten only on a definitive outcome: posted, a first-attempt
 *    refusal, or a refusal/discard confirmed by **voiding** the key (after a
 *    void, the key can never post — so "not sent" is a guarantee, not a guess);
 *  - records are per user; operations only ever touch the current user's.
 *
 * @param currentUser The signed-in user id, or null when signed out.
 * @param execute Sends the payment with its key: transfer, check pay or FX by [PendingPayment.kind].
 * @param lookup GET /v1/transactions/{id} — read-only status of a key.
 * @param void POST /v1/transactions/{id}/void — guarantees the key never posts afterwards.
 * @param io Where the (synchronous, disk-hitting) store calls run.
 */
class PaymentSubmitter(
    private val store: PendingPaymentStore,
    private val currentUser: () -> String?,
    private val newKey: () -> String,
    private val authorizer: PaymentAuthorizer,
    private val execute: suspend (PendingPayment) -> ApiOutcome<Settlement>,
    private val lookup: suspend (transactionId: String) -> ApiOutcome<TransactionStatusDto>,
    private val void: suspend (transactionId: String) -> ApiOutcome<TransactionStatusDto>,
    private val clock: () -> Long = System::currentTimeMillis,
    private val io: CoroutineDispatcher = Dispatchers.IO,
) {
    /** One operation at a time: two submissions must never race for one user's slot. */
    private val mutex = Mutex()

    private val _pending = MutableStateFlow<PendingPayment?>(null)

    /**
     * The current user's unsettled payment as last seen by this submitter.
     * Refreshed by [pending] and by every operation; reset by [forget].
     */
    val pendingFlow: StateFlow<PendingPayment?> = _pending.asStateFlow()

    /**
     * The current user's unresolved payment from a previous attempt/app run, if
     * any. Null also when signed out or when the store cannot be read (in which
     * case [submitNew] refuses too, so nothing can race an unreadable record).
     */
    suspend fun pending(): PendingPayment? {
        val user = signedInUser() ?: return null.also { _pending.value = null }
        return try {
            load(user)
        } catch (_: LocalStorageException) {
            null
        }.also { _pending.value = it }
    }

    /** Sign-out: drop the in-memory view (the record itself stays with its owner). */
    fun forget() {
        _pending.value = null
    }

    /**
     * Authorize, persist and submit a payment the user just confirmed.
     * [SubmitResult.Blocked] while this user has an earlier unsettled payment.
     */
    suspend fun submitNew(
        fromAccount: String,
        toAccount: String,
        amountMinor: Long,
        currency: String,
        recipientLabel: String,
        checkId: String? = null,
        kind: PaymentKind = if (checkId != null) PaymentKind.CHECK else PaymentKind.TRANSFER,
    ): SubmitResult = mutex.withLock {
        val user = signedInUser() ?: return SubmitResult.NotStarted
        val existing = try {
            load(user)
        } catch (_: LocalStorageException) {
            // Can't see whether a payment is pending: never start a second one blind.
            return SubmitResult.NotStarted
        }
        if (existing != null) {
            _pending.value = existing
            return SubmitResult.Blocked(existing)
        }
        val draft = PendingPayment(
            idempotencyKey = newKey(),
            fromAccount = fromAccount,
            toAccount = toAccount,
            amountMinor = amountMinor,
            currency = currency,
            recipientLabel = recipientLabel,
            checkId = checkId,
            userId = user,
            kind = kind,
            createdAtMs = clock(),
        )
        val payment = when (val approval = authorizer.authorize(draft)) {
            is Authorization.Granted -> draft.copy(authorization = approval.signature)
            Authorization.Cancelled -> return SubmitResult.NotAuthorized(denial = null)
            is Authorization.Denied -> return SubmitResult.NotAuthorized(approval.reason)
        }
        // Durable BEFORE the first attempt — and if it isn't, the attempt never
        // happens: a key that exists only in memory can be lost mid-flight.
        if (!save(payment)) return SubmitResult.NotStarted
        _pending.value = payment
        attempt(payment, isRetry = false)
    }

    /**
     * Retry the current user's stored payment with its original key (and its
     * original approval — a retry is the same intent, not a new one). Null if
     * nothing is pending.
     */
    suspend fun retryPending(): SubmitResult? = mutex.withLock {
        val payment = loadCurrentOr { return SubmitResult.NotStarted } ?: return null
        attempt(payment, isRetry = true)
    }

    /**
     * Read-only: has the pending payment posted? Clears the record when the
     * answer is definitive (posted / voided). Moves no money, needs no approval
     * — safe to run automatically on Home and at startup. Null if nothing pending.
     */
    suspend fun checkPending(): PendingCheck? = mutex.withLock {
        val payment = loadCurrentOr { return null } ?: return null
        when (val status = lookup(payment.idempotencyKey)) {
            is ApiOutcome.Ok -> when {
                status.value.isPosted -> {
                    clear(payment)
                    PendingCheck.Posted(payment, status.value.transactionId)
                }
                status.value.isVoided -> {
                    clear(payment)
                    PendingCheck.Voided(payment)
                }
                else -> PendingCheck.Unknown(payment, offline = false)
            }
            is ApiOutcome.Failed ->
                if (status.codedNotFound) PendingCheck.NotPosted(payment) else PendingCheck.Unknown(payment, offline = false)
            is ApiOutcome.Offline -> PendingCheck.Unknown(payment, offline = true)
        }
    }

    /**
     * Drop the current user's unsettled payment on an explicit user decision —
     * by **voiding** its key first: `voided` guarantees it can never post, so
     * dropping is safe; `posted` means it went through and we say so instead;
     * anything inconclusive keeps the record. Never called by program logic.
     */
    suspend fun discardPending(): DiscardResult? = mutex.withLock {
        val payment = loadCurrentOr { return DiscardResult.CouldNotVerify(offline = false) } ?: return null
        when (val voided = void(payment.idempotencyKey)) {
            is ApiOutcome.Ok -> when {
                voided.value.isPosted -> {
                    clear(payment)
                    DiscardResult.WasPosted(voided.value.transactionId)
                }
                voided.value.isVoided -> {
                    clear(payment)
                    DiscardResult.Discarded
                }
                else -> DiscardResult.CouldNotVerify(offline = false)
            }
            is ApiOutcome.Failed -> if (voided.codedNotFound) {
                // The key belongs to a transaction none of this user's accounts
                // is in (a record from before per-user binding, adopted by the
                // wrong user): nothing of theirs was or can be sent with it.
                clear(payment)
                DiscardResult.Discarded
            } else {
                DiscardResult.CouldNotVerify(offline = false)
            }
            is ApiOutcome.Offline -> DiscardResult.CouldNotVerify(offline = true)
        }
    }

    // --- internals ---

    private fun signedInUser(): String? = try {
        currentUser()?.takeIf { it.isNotBlank() }
    } catch (_: LocalStorageException) {
        null
    }

    /** The current user's record; [onFault] decides what an unreadable store means. */
    private suspend inline fun loadCurrentOr(onFault: () -> Nothing): PendingPayment? {
        val user = signedInUser() ?: return null
        return try {
            load(user)
        } catch (_: LocalStorageException) {
            onFault()
        }.also { _pending.value = it }
    }

    // The store is synchronous by contract (persist-before-network must be
    // observable); these keep its disk I/O off the caller's (UI) thread.
    private suspend fun load(userId: String): PendingPayment? = withContext(io) { store.load(userId) }

    private suspend fun save(payment: PendingPayment): Boolean = withContext(io) {
        try {
            store.save(payment)
        } catch (_: LocalStorageException) {
            false
        }
    }

    /**
     * A failed clear is harmless: the record resurfaces and its next retry or
     * check is answered by the replay / the status endpoint.
     */
    private suspend fun clear(payment: PendingPayment) {
        withContext(io) {
            try {
                store.clear(payment.userId, payment.idempotencyKey)
            } catch (_: LocalStorageException) {
                // see above
            }
        }
        if (_pending.value?.idempotencyKey == payment.idempotencyKey) _pending.value = null
    }

    private suspend fun attempt(payment: PendingPayment, isRetry: Boolean): SubmitResult =
        when (val outcome = execute(payment)) {
            is ApiOutcome.Ok -> {
                clear(payment)
                SubmitResult.Posted(outcome.value.transactionId, outcome.value.alreadyPosted, outcome.value.fx)
            }
            is ApiOutcome.Failed -> when {
                // 2xx-unreadable, 5xx, retry_later, 429: the server didn't rule
                // (or ruled and we lost the answer). It may yet post on retry.
                outcome.undetermined -> SubmitResult.Unsettled(offline = false)
                // The key was voided (e.g. a Discard whose answer was lost): it
                // can never post. Definitive.
                outcome.code == ErrorCode.VOIDED -> {
                    clear(payment)
                    SubmitResult.Rejected(ErrorCode.VOIDED, outcome.serverMessage)
                }
                // The transaction id (= our key) already exists in the ledger —
                // e.g. a retry after the server's idempotency retention expired.
                // That is "look it up", never "rejected".
                outcome.code == ErrorCode.DUPLICATE_TRANSACTION -> resolveByLookup(payment)
                // The session is gone; nothing can be confirmed until sign-in.
                isRetry && outcome.code == ErrorCode.UNAUTHORIZED -> SubmitResult.Unsettled(offline = false)
                // A refusal of a RETRY says nothing certain about the FIRST
                // attempt (it may still be in flight, or have been answered by
                // a gate before the replay). Void the key: afterwards it can
                // never post, so the refusal becomes a guarantee — or we learn
                // that it did post.
                isRetry -> reconcileByVoid(payment, outcome)
                // A first answer that refuses: the server ruled on this key.
                else -> {
                    clear(payment)
                    SubmitResult.Rejected(outcome.code, outcome.serverMessage)
                }
            }
            is ApiOutcome.Offline -> SubmitResult.Unsettled(offline = true)
        }

    private suspend fun resolveByLookup(payment: PendingPayment): SubmitResult =
        when (val status = lookup(payment.idempotencyKey)) {
            is ApiOutcome.Ok -> when {
                status.value.isPosted -> {
                    clear(payment)
                    SubmitResult.Posted(status.value.transactionId, alreadyPosted = true)
                }
                status.value.isVoided -> {
                    clear(payment)
                    SubmitResult.Rejected(ErrorCode.VOIDED, null)
                }
                else -> SubmitResult.Unsettled(offline = false)
            }
            // Including a 404: we can't tell — keep it; Discard (void) can settle it.
            is ApiOutcome.Failed -> SubmitResult.Unsettled(offline = false)
            is ApiOutcome.Offline -> SubmitResult.Unsettled(offline = true)
        }

    private suspend fun reconcileByVoid(payment: PendingPayment, refusal: ApiOutcome.Failed): SubmitResult =
        when (val voided = void(payment.idempotencyKey)) {
            is ApiOutcome.Ok -> when {
                voided.value.isPosted -> {
                    clear(payment)
                    SubmitResult.Posted(voided.value.transactionId, alreadyPosted = true)
                }
                voided.value.isVoided -> {
                    clear(payment)
                    SubmitResult.Rejected(refusal.code, refusal.serverMessage)
                }
                else -> SubmitResult.Unsettled(offline = false)
            }
            is ApiOutcome.Failed -> if (voided.codedNotFound) {
                // Not this user's transaction: nothing of theirs posted with it.
                clear(payment)
                SubmitResult.Rejected(refusal.code, refusal.serverMessage)
            } else {
                // Can't confirm: keep the key rather than forget a payment that
                // may have posted. The user can retry, or discard (which voids).
                SubmitResult.Unsettled(offline = false)
            }
            is ApiOutcome.Offline -> SubmitResult.Unsettled(offline = true)
        }
}
