package tj.payment.wallet.ui.send

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.Currency
import tj.payment.core.DiscardResult
import tj.payment.core.ErrorCode
import tj.payment.core.Money
import tj.payment.core.PendingPayment
import tj.payment.core.ResolveResponse
import tj.payment.core.SubmitResult
import tj.payment.core.WalletDto
import tj.payment.core.transferFeePreviewMinor
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.NOT_STARTED_MESSAGE
import tj.payment.wallet.ui.userMessage
import tj.payment.wallet.R
import tj.payment.wallet.ui.Copy

enum class SendStep { RECIPIENT, AMOUNT, CONFIRM, RESULT }

/** Terminal (or retryable) outcome shown on the RESULT step. */
sealed interface SendOutcome {
    data class Success(val alreadyPosted: Boolean) : SendOutcome

    /** [code] lets the screen offer the fix — `kyc_required` → "Verify now". */
    data class Rejected(val message: String, val code: ErrorCode? = null) : SendOutcome

    /** Outcome unknown (offline/5xx/429/unreadable 2xx) — same-key retry is the only exit. */
    data class Unsettled(val offline: Boolean) : SendOutcome
}

data class SendUiState(
    val step: SendStep = SendStep.RECIPIENT,
    /** An unfinished payment from a previous run — must be resolved first. */
    val pendingResume: PendingPayment? = null,
    /** The Discard confirmation dialog is open. */
    val confirmDiscard: Boolean = false,
    /** Discard is checking the statement before honouring the request. */
    val discarding: Boolean = false,
    /** Why the pending payment could not be discarded just now. */
    val pendingError: String? = null,

    val phone: String = "",
    val resolving: Boolean = false,
    val recipient: ResolveResponse? = null,
    val recipientError: String? = null,

    val fromWallet: WalletDto? = null,
    val amountText: String = "",
    /** Fee in bps from GET /v1/config; null while unknown (fee line hidden). */
    val feeBps: Int? = null,

    val submitting: Boolean = false,
    /** Why the device did not approve the payment (shown on the confirm step); nothing was sent. */
    val authError: String? = null,
    val outcome: SendOutcome? = null,
    /** What the user confirmed — kept for the result screen. */
    val confirmedAmount: Money? = null,
    val confirmedLabel: String = "",
) {
    val phoneDigits: String get() = phone.filter { it.isDigit() }
    val canCheckPhone: Boolean get() = !resolving && phoneDigits.length in 7..15

    val amount: Money? get() = fromWallet?.let { Money.parse(amountText, Currency.of(it.currency)) }

    val insufficient: Boolean
        get() = amount?.let { a -> fromWallet?.let { a.minorUnits > it.balanceMinor } } == true

    /** Display-only fee line; TJS only, exactly like the backend (null = no fee line). */
    val feeMinor: Long?
        get() = amount?.let { transferFeePreviewMinor(it.minorUnits, it.currency.code, feeBps) }

    val canContinueAmount: Boolean
        get() = amount?.isPositive == true && !insufficient

    val recipientLabel: String
        get() = recipient?.name ?: "+${phoneDigits}"
}

/**
 * @param savedState Keeps what the user TYPED (phone, amount) across process
 *   death, so a recreated Send screen does not start blank. Only inputs are
 *   saved: the resolved recipient, wallet and step are re-derived (a "Check
 *   number" tap) rather than restored — the network answer may have changed.
 */
class SendViewModel(
    private val repo: WalletRepository,
    private val savedState: SavedStateHandle = SavedStateHandle(),
) : ViewModel() {

    private val _state = MutableStateFlow(
        SendUiState(
            phone = savedState[KEY_PHONE] ?: "",
            amountText = savedState[KEY_AMOUNT] ?: "",
        ),
    )
    val state: StateFlow<SendUiState> = _state.asStateFlow()

    init {
        viewModelScope.launch {
            // An unsettled payment from a previous run blocks a new one (one key,
            // one intent) — surface it before anything else on this screen.
            repo.submitter.pending()?.let { pending ->
                _state.update { it.copy(pendingResume = pending) }
            }
            // Both come from the repository cache when Home fetched them moments ago.
            val walletsDeferred = async { repo.wallets() }
            val configDeferred = async { repo.config() }
            val wallets = (walletsDeferred.await() as? ApiOutcome.Ok)?.value.orEmpty()
            val feeBps = (configDeferred.await() as? ApiOutcome.Ok)?.value?.transferFeeBps
            _state.update {
                it.copy(
                    // Sends are TJS-first: the recipient resolve returns their TJS wallet.
                    fromWallet = wallets.firstOrNull { w -> w.currency == "TJS" } ?: wallets.firstOrNull(),
                    feeBps = feeBps,
                )
            }
        }
    }

    fun onPhoneChange(value: String) {
        savedState[KEY_PHONE] = value
        _state.update { it.copy(phone = value, recipientError = null, recipient = null) }
    }

    /** The "check number" step: resolve before any amount is typed. */
    fun checkRecipient() {
        val s = _state.value
        if (!s.canCheckPhone) return
        _state.value = s.copy(resolving = true, recipientError = null)
        viewModelScope.launch {
            when (val outcome = repo.resolveByPhone(_state.value.phoneDigits)) {
                is ApiOutcome.Ok -> _state.update {
                    it.copy(resolving = false, recipient = outcome.value, step = SendStep.AMOUNT)
                }
                is ApiOutcome.Failed -> _state.update {
                    it.copy(
                        resolving = false,
                        recipientError = if (outcome.code == ErrorCode.NOT_FOUND) {
                            Copy.text(R.string.send_error_no_wallet)
                        } else {
                            outcome.userMessage()
                        },
                    )
                }
                is ApiOutcome.Offline -> _state.update {
                    it.copy(resolving = false, recipientError = outcome.userMessage())
                }
            }
        }
    }

    // --- Amount keypad ---

    fun keyDigit(d: Char) {
        val s = _state.value
        val text = s.amountText
        // Don't allow amounts no currency can hold, or more decimals than TJS has.
        val decimals = text.substringAfter(',', "")
        if (text.contains(',') && decimals.length >= 2) return
        if (!text.contains(',') && text.length >= 12) return
        if (text == "0" && d != ',') {
            setAmountText(d.toString())
            return
        }
        setAmountText(text + d)
    }

    fun keyComma() {
        val s = _state.value
        if (s.amountText.contains(',')) return
        setAmountText(if (s.amountText.isEmpty()) "0," else s.amountText + ",")
    }

    fun keyBackspace() {
        val s = _state.value
        if (s.amountText.isEmpty()) return
        setAmountText(s.amountText.dropLast(1))
    }

    private fun setAmountText(value: String) {
        savedState[KEY_AMOUNT] = value
        _state.update { it.copy(amountText = value) }
    }

    fun toConfirm() {
        val s = _state.value
        if (!s.canContinueAmount) return
        _state.value = s.copy(step = SendStep.CONFIRM, authError = null)
    }

    fun backTo(step: SendStep) {
        if (_state.value.submitting) return
        _state.update { it.copy(step = step) }
    }

    // --- Submission (all through the PaymentSubmitter machine) ---

    /**
     * The submitter first asks the device owner to approve (fingerprint/face
     * or the screen lock, bound to a Keystore key), then persists and sends.
     */
    fun confirmAndSend() {
        val s = _state.value
        val from = s.fromWallet ?: return
        val to = s.recipient ?: return
        val amount = s.amount ?: return
        if (s.submitting) return
        _state.value = s.copy(
            submitting = true,
            authError = null,
            confirmedAmount = amount,
            confirmedLabel = s.recipientLabel,
        )
        viewModelScope.launch {
            val result = repo.submitter.submitNew(
                fromAccount = from.id,
                toAccount = to.walletId,
                amountMinor = amount.minorUnits,
                currency = from.currency,
                recipientLabel = _state.value.recipientLabel,
            )
            settle(result)
        }
    }

    /** Retry the SAME payment with the SAME idempotency key. */
    fun retry() {
        if (_state.value.submitting) return
        _state.update { it.copy(submitting = true, step = SendStep.RESULT) }
        viewModelScope.launch { settle(repo.submitter.retryPending()) }
    }

    /** Resume the pending payment surfaced at open (same machine as [retry]). */
    fun resumePending() {
        val p = _state.value.pendingResume ?: return
        _state.update {
            it.copy(
                pendingResume = null,
                pendingError = null,
                step = SendStep.RESULT,
                submitting = true,
                confirmedAmount = Money.ofMinor(p.amountMinor, Currency.of(p.currency)),
                confirmedLabel = p.recipientLabel,
            )
        }
        viewModelScope.launch { settle(repo.submitter.retryPending()) }
    }

    /** Discard is destructive for the audit trail: ask first. */
    fun requestDiscard() {
        if (_state.value.discarding) return
        _state.update { it.copy(confirmDiscard = true, pendingError = null) }
    }

    fun cancelDiscard() {
        _state.update { it.copy(confirmDiscard = false) }
    }

    /**
     * The user confirmed. The submitter voids the key with the server first:
     * afterwards it can never post, so dropping it is safe; a payment that did
     * post is shown as sent instead of silently forgotten; and one whose void
     * got no answer is kept.
     */
    fun confirmDiscard() {
        val p = _state.value.pendingResume ?: return
        _state.update { it.copy(confirmDiscard = false, discarding = true, pendingError = null) }
        viewModelScope.launch {
            when (val result = repo.submitter.discardPending()) {
                null, DiscardResult.Discarded -> _state.update {
                    it.copy(discarding = false, pendingResume = null)
                }
                is DiscardResult.WasPosted -> _state.update {
                    it.copy(
                        discarding = false,
                        pendingResume = null,
                        step = SendStep.RESULT,
                        confirmedAmount = Money.ofMinor(p.amountMinor, Currency.of(p.currency)),
                        confirmedLabel = p.recipientLabel,
                        outcome = SendOutcome.Success(alreadyPosted = true),
                    )
                }
                is DiscardResult.CouldNotVerify -> _state.update {
                    it.copy(
                        discarding = false,
                        pendingError = if (result.offline) {
                            Copy.text(R.string.pending_notice_discard_failed_offline)
                        } else {
                            Copy.text(R.string.pending_notice_discard_failed)
                        },
                    )
                }
            }
        }
    }

    private fun settle(result: SubmitResult?) {
        val outcome = when (result) {
            null -> SendOutcome.Rejected(Copy.text(R.string.payment_nothing_to_submit))
            is SubmitResult.Posted -> SendOutcome.Success(result.alreadyPosted)
            is SubmitResult.Rejected -> SendOutcome.Rejected(result.code.userMessage(), result.code)
            is SubmitResult.Unsettled -> SendOutcome.Unsettled(result.offline)
            SubmitResult.NotStarted -> SendOutcome.Rejected(NOT_STARTED_MESSAGE)
            // Not approved on the device: nothing stored, nothing sent. Stay on
            // the confirm step (a dismissed prompt needs no message).
            is SubmitResult.NotAuthorized -> {
                _state.update {
                    it.copy(submitting = false, step = SendStep.CONFIRM, authError = result.denial?.userMessage())
                }
                return
            }
            // This user already has an unsettled payment: show it (finish or
            // discard) before anything new — one key per intent.
            is SubmitResult.Blocked -> {
                _state.update {
                    it.copy(submitting = false, step = SendStep.RECIPIENT, pendingResume = result.pending, outcome = null)
                }
                return
            }
        }
        _state.update {
            it.copy(submitting = false, step = SendStep.RESULT, outcome = outcome)
        }
    }

    private companion object {
        const val KEY_PHONE = "send.phone"
        const val KEY_AMOUNT = "send.amount"
    }
}
