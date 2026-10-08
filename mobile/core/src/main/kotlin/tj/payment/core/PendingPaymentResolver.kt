package tj.payment.core

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/**
 * Screen-agnostic driver for "this user has an unsettled payment" (FRONTEND.md
 * §2.2: resolve it at startup, don't wait for Send to open). A host screen
 * (Home, FX) creates one with its own scope and renders [state]:
 *
 *  - [load] shows the current user's pending payment and — once per call —
 *    asks the server whether it already posted (read-only, no approval needed),
 *    clearing it silently when the answer is definitive;
 *  - [finish] retries it with its original key (same approval);
 *  - [requestDiscard] / [confirmDiscard] void it (never a blind drop).
 *
 * All decisions live in [PaymentSubmitter]; this class only sequences them and
 * turns results into [Notice]s a screen can phrase.
 *
 * @param onResolved Called whenever the pending record went away (sent,
 *   cancelled or refused), so the host can refresh balances.
 */
class PendingPaymentResolver(
    private val submitter: PaymentSubmitter,
    private val scope: CoroutineScope,
    private val onResolved: () -> Unit = {},
) {
    data class State(
        /** The current user's unsettled payment, or null. */
        val pending: PendingPayment? = null,
        val busy: Busy? = null,
        /** The discard confirmation is showing. */
        val confirmDiscard: Boolean = false,
        /** The last thing worth telling the user; cleared by [dismissNotice]. */
        val notice: Notice? = null,
    )

    enum class Busy { CHECKING, RETRYING, DISCARDING }

    sealed interface Notice {
        /** It went through (found by a status check, a retry, or a discard that found it posted). */
        data class Sent(val payment: PendingPayment, val fx: FxResponse? = null) : Notice

        /** The key is voided: it can never go through. Nothing was sent. */
        data class Cancelled(val payment: PendingPayment) : Notice

        /** The server refused it, definitively. Nothing was sent. */
        data class Refused(val payment: PendingPayment, val code: ErrorCode) : Notice

        /** A retry got no definitive answer; it is still pending. */
        data class NoAnswer(val offline: Boolean) : Notice

        /** Discard could not get a definitive answer; it is still pending. */
        data class DiscardFailed(val offline: Boolean) : Notice

        /** The device's secure storage could not be read or written. */
        data object StorageProblem : Notice
    }

    private val _state = MutableStateFlow(State())
    val state: StateFlow<State> = _state.asStateFlow()

    /**
     * Show the current user's pending payment. With [check], ask the server
     * once whether it already posted / was voided and clear it if so.
     */
    fun load(check: Boolean = true) {
        if (_state.value.busy != null) return
        scope.launch {
            val pending = submitter.pending()
            _state.update { it.copy(pending = pending, confirmDiscard = it.confirmDiscard && pending != null) }
            if (pending == null || !check) return@launch
            _state.update { it.copy(busy = Busy.CHECKING) }
            when (val checked = submitter.checkPending()) {
                null -> _state.update { it.copy(busy = null, pending = null) }
                is PendingCheck.Posted -> resolved(Notice.Sent(checked.payment))
                is PendingCheck.Voided -> resolved(Notice.Cancelled(checked.payment))
                // Still unconfirmed: the card itself says so; no extra notice.
                is PendingCheck.NotPosted, is PendingCheck.Unknown ->
                    _state.update { it.copy(busy = null, pending = checked.payment) }
            }
        }
    }

    /** Retry the pending payment with its original key. */
    fun finish() {
        val s = _state.value
        val payment = s.pending ?: return
        if (s.busy != null) return
        _state.update { it.copy(busy = Busy.RETRYING, notice = null, confirmDiscard = false) }
        scope.launch {
            when (val result = submitter.retryPending()) {
                null -> _state.update { it.copy(busy = null, pending = null) }
                is SubmitResult.Posted -> resolved(Notice.Sent(payment, result.fx))
                is SubmitResult.Rejected -> resolved(
                    if (result.code == ErrorCode.VOIDED) Notice.Cancelled(payment) else Notice.Refused(payment, result.code),
                )
                is SubmitResult.Unsettled -> stillPending(Notice.NoAnswer(result.offline))
                SubmitResult.NotStarted -> stillPending(Notice.StorageProblem)
                // Not produced by a retry; keep the card as it is.
                is SubmitResult.NotAuthorized, is SubmitResult.Blocked -> stillPending(null)
            }
        }
    }

    fun requestDiscard() {
        val s = _state.value
        if (s.pending == null || s.busy != null) return
        _state.update { it.copy(confirmDiscard = true, notice = null) }
    }

    fun cancelDiscard() {
        _state.update { it.copy(confirmDiscard = false) }
    }

    /** The user confirmed: void the key, then drop the record (or learn it posted). */
    fun confirmDiscard() {
        val s = _state.value
        val payment = s.pending ?: return
        if (s.busy != null) return
        _state.update { it.copy(busy = Busy.DISCARDING, confirmDiscard = false, notice = null) }
        scope.launch {
            when (val result = submitter.discardPending()) {
                null -> _state.update { it.copy(busy = null, pending = null) }
                is DiscardResult.WasPosted -> resolved(Notice.Sent(payment))
                DiscardResult.Discarded -> resolved(Notice.Cancelled(payment))
                is DiscardResult.CouldNotVerify -> stillPending(Notice.DiscardFailed(result.offline))
            }
        }
    }

    fun dismissNotice() {
        _state.update { it.copy(notice = null) }
    }

    private suspend fun stillPending(notice: Notice?) {
        val pending = submitter.pending()
        _state.update { it.copy(busy = null, pending = pending, notice = notice) }
        if (pending == null) onResolved()
    }

    private suspend fun resolved(notice: Notice) {
        val pending = submitter.pending() // normally null; a store fault keeps it showing
        _state.update { it.copy(busy = null, pending = pending, notice = notice, confirmDiscard = false) }
        onResolved()
    }
}
