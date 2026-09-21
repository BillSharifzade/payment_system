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
import tj.payment.core.transferFeeMinor
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

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

    val feeMinor: Long?
        get() = feeBps?.let { bps -> amount?.let { transferFeeMinor(it.minorUnits, bps) } }

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
                            "No wallet with that number."
                        } else {
                            outcome.userMessage()
                        },
                    )
                }
                is ApiOutcome.Offline -> _state.update {
                    it.copy(resolving = false, recipientError = OFFLINE_MESSAGE)
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
        _state.value = s.copy(step = SendStep.CONFIRM)
    }

    fun backTo(step: SendStep) {
        if (_state.value.submitting) return
        _state.update { it.copy(step = step) }
    }

    // --- Submission (all through the PaymentSubmitter machine) ---

    fun confirmAndSend() {
        val s = _state.value
        val from = s.fromWallet ?: return
        val to = s.recipient ?: return
        val amount = s.amount ?: return
        if (s.submitting) return
        _state.value = s.copy(
            submitting = true,
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
     * The user confirmed. The submitter checks the statement first: a payment
     * that did post is shown as sent instead of silently forgotten, and one
     * that can't be checked is kept.
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
                            "Can't check whether it went through while offline. Connect and try again."
                        } else {
                            "Couldn't check whether it went through. Try again in a moment."
                        },
                    )
                }
            }
        }
    }

    private fun settle(result: SubmitResult?) {
        val outcome = when (result) {
            null -> SendOutcome.Rejected("Nothing to submit.")
            is SubmitResult.Posted -> SendOutcome.Success(result.alreadyPosted)
            is SubmitResult.Rejected -> SendOutcome.Rejected(result.code.userMessage(), result.code)
            is SubmitResult.Unsettled -> SendOutcome.Unsettled(result.offline)
            SubmitResult.NotStarted -> SendOutcome.Rejected(
                "Couldn't save this payment on your device, so nothing was sent. Please try again.",
            )
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
