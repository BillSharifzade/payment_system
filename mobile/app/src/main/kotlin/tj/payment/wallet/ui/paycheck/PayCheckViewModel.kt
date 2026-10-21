package tj.payment.wallet.ui.paycheck

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.CheckDto
import tj.payment.core.Currency
import tj.payment.core.ErrorCode
import tj.payment.core.Money
import tj.payment.core.SubmitResult
import tj.payment.core.WalletDto
import tj.payment.core.transferFeePreviewMinor
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.NOT_STARTED_MESSAGE
import tj.payment.wallet.ui.userMessage
import tj.payment.wallet.R
import tj.payment.wallet.ui.Copy

enum class PayStep { SCAN, PREVIEW, RESULT }

sealed interface PayOutcome {
    data class Success(val alreadyPosted: Boolean) : PayOutcome
    data class Rejected(val message: String, val code: ErrorCode? = null) : PayOutcome
    data class Unsettled(val offline: Boolean) : PayOutcome
}

data class PayCheckUiState(
    val step: PayStep = PayStep.SCAN,
    val codeText: String = "",
    val lookingUp: Boolean = false,
    val scanError: String? = null,

    val check: CheckDto? = null,
    val fromWallet: WalletDto? = null,
    val feeBps: Int? = null,
    /** Why the biometric gate refused just now (shown under the pay button). */
    val gateMessage: String? = null,

    val submitting: Boolean = false,
    val outcome: PayOutcome? = null,
    val confirmedAmount: Money? = null,
    val confirmedLabel: String = "",
) {
    val canLookUp: Boolean get() = !lookingUp && CheckCode.parse(codeText) != null

    val amount: Money? get() = check?.money()

    val insufficient: Boolean
        get() = check?.let { c -> fromWallet?.let { c.amountMinor > it.balanceMinor } } == true

    /** Display-only (paid by the merchant); TJS only, exactly like the backend. */
    val feeMinor: Long?
        get() = check?.let { transferFeePreviewMinor(it.amountMinor, it.currency, feeBps) }

    val merchantLabel: String
        get() = check?.merchantName ?: Copy.text(R.string.pay_unverified_merchant)

    val canPay: Boolean get() = check?.isOpen == true && fromWallet != null && !insufficient && !submitting
}

/**
 * Pay-by-QR (DESIGN.md §20 path B): scan or type a check code, preview what
 * the merchant asks for, confirm with the device biometric gate, and settle
 * through the same PaymentSubmitter machine as Send — persisted key first, one
 * charge at most, unsettled outcomes retried with the same key.
 */
class PayCheckViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(PayCheckUiState())
    val state: StateFlow<PayCheckUiState> = _state.asStateFlow()

    init {
        viewModelScope.launch {
            // An unsettled payment from a previous run must be finished (same
            // key) before a new one: land straight on its retry screen.
            repo.submitter.pending()?.let { pending ->
                _state.update {
                    it.copy(
                        step = PayStep.RESULT,
                        outcome = PayOutcome.Unsettled(offline = false),
                        confirmedAmount = Money.ofMinor(pending.amountMinor, Currency.of(pending.currency)),
                        confirmedLabel = pending.recipientLabel,
                    )
                }
            }
            val walletsDeferred = async { repo.wallets() }
            val configDeferred = async { repo.config() }
            val wallets = (walletsDeferred.await() as? ApiOutcome.Ok)?.value.orEmpty()
            val feeBps = (configDeferred.await() as? ApiOutcome.Ok)?.value?.transferFeeBps
            _state.update { it.copy(fromWallet = pickWallet(wallets, it.check?.currency), feeBps = feeBps) }
        }
    }

    private fun pickWallet(wallets: List<WalletDto>, currency: String?): WalletDto? {
        val wanted = currency ?: "TJS"
        // Same rule as the server's default: the fattest wallet in the check's currency.
        return wallets.filter { it.currency == wanted }.maxByOrNull { it.balanceMinor }
    }

    fun onCodeChange(value: String) {
        _state.update { it.copy(codeText = value, scanError = null) }
    }

    /** A QR scan result (or a paste): look the check up right away. */
    fun onScanned(raw: String) {
        val id = CheckCode.parse(raw)
        if (id == null) {
            _state.update { it.copy(scanError = Copy.text(R.string.pay_error_not_a_check)) }
            return
        }
        _state.update { it.copy(codeText = id, scanError = null) }
        lookUp()
    }

    fun lookUp() {
        val id = CheckCode.parse(_state.value.codeText) ?: return
        if (_state.value.lookingUp) return
        _state.update { it.copy(lookingUp = true, scanError = null) }
        viewModelScope.launch {
            when (val outcome = repo.check(id)) {
                is ApiOutcome.Ok -> {
                    val check = outcome.value
                    val wallets = (repo.wallets() as? ApiOutcome.Ok)?.value.orEmpty()
                    _state.update {
                        it.copy(
                            lookingUp = false,
                            check = check,
                            fromWallet = pickWallet(wallets, check.currency),
                            step = PayStep.PREVIEW,
                            gateMessage = null,
                        )
                    }
                }
                is ApiOutcome.Failed -> _state.update {
                    it.copy(
                        lookingUp = false,
                        scanError = when (outcome.code) {
                            ErrorCode.NOT_FOUND -> Copy.text(R.string.pay_error_no_open_check)
                            else -> outcome.userMessage()
                        },
                    )
                }
                is ApiOutcome.Offline -> _state.update { it.copy(lookingUp = false, scanError = outcome.userMessage()) }
            }
        }
    }

    fun backToScan() {
        if (_state.value.submitting) return
        _state.update { it.copy(step = PayStep.SCAN, check = null, outcome = null, gateMessage = null) }
    }

    /**
     * Pay the previewed check. The submitter asks the device owner to approve
     * (fingerprint/face or screen lock, bound to a Keystore key), then persists
     * the key and submits.
     */
    fun pay() {
        val s = _state.value
        val check = s.check ?: return
        val from = s.fromWallet ?: return
        if (!s.canPay) return
        _state.value = s.copy(
            submitting = true,
            gateMessage = null,
            confirmedAmount = check.money(),
            confirmedLabel = s.merchantLabel,
        )
        viewModelScope.launch {
            val result = repo.submitter.submitNew(
                fromAccount = from.id,
                toAccount = check.account,
                amountMinor = check.amountMinor,
                currency = check.currency,
                recipientLabel = s.merchantLabel,
                checkId = check.id,
            )
            settle(result)
        }
    }

    /** Retry the SAME payment with the SAME idempotency key. */
    fun retry() {
        if (_state.value.submitting) return
        _state.update { it.copy(submitting = true, step = PayStep.RESULT) }
        viewModelScope.launch { settle(repo.submitter.retryPending()) }
    }

    private fun settle(result: SubmitResult?) {
        val outcome = when (result) {
            null -> PayOutcome.Rejected(Copy.text(R.string.payment_nothing_to_submit))
            is SubmitResult.Posted -> PayOutcome.Success(result.alreadyPosted)
            is SubmitResult.Rejected -> PayOutcome.Rejected(rejectionCopy(result.code), result.code)
            is SubmitResult.Unsettled -> PayOutcome.Unsettled(result.offline)
            SubmitResult.NotStarted -> PayOutcome.Rejected(NOT_STARTED_MESSAGE)
            // Not approved on the device: nothing stored, nothing sent; back to
            // the preview (a dismissed prompt needs no message).
            is SubmitResult.NotAuthorized -> {
                _state.update { it.copy(submitting = false, gateMessage = result.denial?.userMessage()) }
                return
            }
            // An earlier payment of this user is unsettled: it must be finished
            // first (same key) — show it, exactly like opening this screen does.
            is SubmitResult.Blocked -> {
                _state.update {
                    it.copy(
                        submitting = false,
                        step = PayStep.RESULT,
                        outcome = PayOutcome.Unsettled(offline = false),
                        confirmedAmount = Money.ofMinor(result.pending.amountMinor, Currency.of(result.pending.currency)),
                        confirmedLabel = result.pending.recipientLabel,
                    )
                }
                return
            }
        }
        _state.update { it.copy(submitting = false, step = PayStep.RESULT, outcome = outcome) }
    }

    private fun rejectionCopy(code: ErrorCode): String = when (code) {
        ErrorCode.CONFLICT -> Copy.text(R.string.pay_error_check_closed)
        ErrorCode.NOT_FOUND -> Copy.text(R.string.pay_error_check_gone)
        ErrorCode.BAD_REQUEST -> Copy.text(R.string.pay_error_wrong_wallet)
        else -> code.userMessage()
    }
}
