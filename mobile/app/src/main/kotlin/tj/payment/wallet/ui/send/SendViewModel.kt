package tj.payment.wallet.ui.send

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.Currency
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
    data class Rejected(val message: String) : SendOutcome
    /** Outcome unknown (offline/5xx/429) — same-key retry is the only exit. */
    data class Unsettled(val offline: Boolean) : SendOutcome
}

data class SendUiState(
    val step: SendStep = SendStep.RECIPIENT,
    /** An unfinished payment from a previous run — must be resolved first. */
    val pendingResume: PendingPayment? = null,

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

class SendViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(SendUiState())
    val state: StateFlow<SendUiState> = _state.asStateFlow()

    init {
        // An unsettled payment from a previous run blocks a new one (one key,
        // one intent) — surface it immediately.
        _state.value = _state.value.copy(pendingResume = repo.submitter.pending())
        viewModelScope.launch {
            val walletsDeferred = async { repo.wallets() }
            val configDeferred = async { repo.config() }
            val wallets = (walletsDeferred.await() as? ApiOutcome.Ok)?.value.orEmpty()
            val feeBps = (configDeferred.await() as? ApiOutcome.Ok)?.value?.transferFeeBps
            _state.value = _state.value.copy(
                // Sends are TJS-first: the recipient resolve returns their TJS wallet.
                fromWallet = wallets.firstOrNull { it.currency == "TJS" } ?: wallets.firstOrNull(),
                feeBps = feeBps,
            )
        }
    }

    fun onPhoneChange(value: String) {
        _state.value = _state.value.copy(phone = value, recipientError = null, recipient = null)
    }

    /** The "check number" step: resolve before any amount is typed. */
    fun checkRecipient() {
        val s = _state.value
        if (!s.canCheckPhone) return
        _state.value = s.copy(resolving = true, recipientError = null)
        viewModelScope.launch {
            when (val outcome = repo.resolveByPhone(_state.value.phoneDigits)) {
                is ApiOutcome.Ok -> _state.value = _state.value.copy(
                    resolving = false,
                    recipient = outcome.value,
                    step = SendStep.AMOUNT,
                )
                is ApiOutcome.Failed -> _state.value = _state.value.copy(
                    resolving = false,
                    recipientError = if (outcome.code == tj.payment.core.ErrorCode.NOT_FOUND) {
                        "No wallet with that number."
                    } else {
                        outcome.userMessage()
                    },
                )
                is ApiOutcome.Offline -> _state.value = _state.value.copy(
                    resolving = false,
                    recipientError = OFFLINE_MESSAGE,
                )
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
            _state.value = s.copy(amountText = d.toString())
            return
        }
        _state.value = s.copy(amountText = text + d)
    }

    fun keyComma() {
        val s = _state.value
        if (s.amountText.contains(',')) return
        _state.value = s.copy(amountText = if (s.amountText.isEmpty()) "0," else s.amountText + ",")
    }

    fun keyBackspace() {
        val s = _state.value
        if (s.amountText.isEmpty()) return
        _state.value = s.copy(amountText = s.amountText.dropLast(1))
    }

    fun toConfirm() {
        val s = _state.value
        if (!s.canContinueAmount) return
        _state.value = s.copy(step = SendStep.CONFIRM)
    }

    fun backTo(step: SendStep) {
        if (_state.value.submitting) return
        _state.value = _state.value.copy(step = step)
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
        _state.value = _state.value.copy(submitting = true, step = SendStep.RESULT)
        viewModelScope.launch { settle(repo.submitter.retryPending()) }
    }

    /** Resume the pending payment surfaced at open (same machine as [retry]). */
    fun resumePending() {
        val p = _state.value.pendingResume ?: return
        _state.value = _state.value.copy(
            pendingResume = null,
            step = SendStep.RESULT,
            submitting = true,
            confirmedAmount = Money.ofMinor(p.amountMinor, Currency.of(p.currency)),
            confirmedLabel = p.recipientLabel,
        )
        viewModelScope.launch { settle(repo.submitter.retryPending()) }
    }

    /** Explicit user decision to drop an unsettled payment. */
    fun discardPending() {
        repo.submitter.abandonPending()
        _state.value = _state.value.copy(pendingResume = null)
    }

    private fun settle(result: SubmitResult?) {
        val outcome = when (result) {
            null -> SendOutcome.Rejected("Nothing to submit.")
            is SubmitResult.Posted -> SendOutcome.Success(result.alreadyPosted)
            is SubmitResult.Rejected -> SendOutcome.Rejected(result.code.userMessage())
            is SubmitResult.Unsettled -> SendOutcome.Unsettled(result.offline)
        }
        _state.value = _state.value.copy(
            submitting = false,
            step = SendStep.RESULT,
            outcome = outcome,
        )
    }
}
