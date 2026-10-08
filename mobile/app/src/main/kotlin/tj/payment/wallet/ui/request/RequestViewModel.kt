package tj.payment.wallet.ui.request

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import java.util.UUID
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.CheckDto
import tj.payment.core.Currency
import tj.payment.core.ErrorCode
import tj.payment.core.IntentKey
import tj.payment.core.Money
import tj.payment.core.WalletDto
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.userMessage
import tj.payment.wallet.R
import tj.payment.wallet.ui.Copy

data class RequestUiState(
    val wallet: WalletDto? = null,
    val amountText: String = "",
    val description: String = "",
    val creating: Boolean = false,
    val error: String? = null,
    /** The open (or just-settled) check being shown as a QR. */
    val check: CheckDto? = null,
    /** Seconds until the shown check expires; 0 once it has. */
    val secondsLeft: Long = 0,
    val cancelling: Boolean = false,
) {
    val amount: Money? get() = wallet?.let { Money.parse(amountText.replace('.', ','), Currency.of(it.currency)) }
    val canCreate: Boolean get() = !creating && wallet != null && amount?.isPositive == true
}

/**
 * The merchant side of pay-by-QR: open a check for an amount, show it as a QR
 * (plus the code as text), and watch it until someone pays it. One key per
 * check: a retried create returns the same check, never a duplicate.
 */
class RequestViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(RequestUiState())
    val state: StateFlow<RequestUiState> = _state.asStateFlow()

    /**
     * One key per check intent (wallet, amount, currency, description): kept
     * across an unknown outcome so a retry can't open two checks; replaced when
     * any input changes (the old key would be answered 409
     * `idempotency_conflict` forever) and dropped after a definitive answer.
     */
    private val checkKey = IntentKey<CheckIntent> { UUID.randomUUID().toString() }

    private data class CheckIntent(val wallet: String, val amountMinor: Long, val currency: String, val description: String?)

    private var watcher: Job? = null

    init {
        viewModelScope.launch {
            val wallets = (repo.wallets() as? ApiOutcome.Ok)?.value.orEmpty()
            _state.update { it.copy(wallet = wallets.firstOrNull { w -> w.currency == "TJS" } ?: wallets.firstOrNull()) }
        }
    }

    fun onAmountChange(value: String) {
        // Digits and one separator only; two decimals at most.
        val cleaned = value.filter { it.isDigit() || it == ',' || it == '.' }
        val sepIndex = cleaned.indexOfFirst { it == ',' || it == '.' }
        val normalized = if (sepIndex >= 0) {
            val head = cleaned.substring(0, sepIndex)
            val tail = cleaned.substring(sepIndex + 1).filter { it.isDigit() }.take(2)
            "$head,$tail"
        } else {
            cleaned
        }
        if (normalized.length > 15) return
        _state.update { it.copy(amountText = normalized, error = null) }
    }

    fun onDescriptionChange(value: String) {
        if (value.length > 140) return
        _state.update { it.copy(description = value) }
    }

    fun create() {
        val s = _state.value
        val wallet = s.wallet ?: return
        val amount = s.amount ?: return
        if (!s.canCreate) return
        _state.update { it.copy(creating = true, error = null) }
        val description = s.description.trim().ifEmpty { null }
        val key = checkKey.keyFor(CheckIntent(wallet.id, amount.minorUnits, wallet.currency, description))
        viewModelScope.launch {
            val outcome = repo.createCheck(
                account = wallet.id,
                amountMinor = amount.minorUnits,
                currency = wallet.currency,
                description = description,
                idempotencyKey = key,
            )
            checkKey.onOutcome(outcome)
            when (outcome) {
                is ApiOutcome.Ok -> {
                    _state.update { it.copy(creating = false, check = outcome.value) }
                    watch(outcome.value)
                }
                is ApiOutcome.Failed -> _state.update {
                    it.copy(
                        creating = false,
                        error = when (outcome.code) {
                            ErrorCode.KYC_REQUIRED -> Copy.text(R.string.request_error_kyc)
                            else -> outcome.userMessage()
                        },
                    )
                }
                is ApiOutcome.Offline -> _state.update { it.copy(creating = false, error = outcome.userMessage()) }
            }
        }
    }

    /** Poll the check while it is on screen: paid/cancelled/expired ends it. */
    private fun watch(check: CheckDto) {
        watcher?.cancel()
        watcher = viewModelScope.launch {
            var current = check
            while (isActive && current.isOpen) {
                val left = (current.expiresAtMs - System.currentTimeMillis()) / 1000
                _state.update { it.copy(secondsLeft = left.coerceAtLeast(0)) }
                delay(POLL_MS)
                when (val fresh = repo.check(current.id)) {
                    is ApiOutcome.Ok -> {
                        current = fresh.value
                        _state.update { it.copy(check = current) }
                    }
                    // A blip: keep showing the QR and try again next tick.
                    is ApiOutcome.Failed, is ApiOutcome.Offline -> Unit
                }
            }
            if (!current.isOpen) {
                _state.update { it.copy(secondsLeft = 0) }
                if (current.status == "paid") repo.markWalletsStale()
            }
        }
    }

    fun cancel() {
        val check = _state.value.check ?: return
        if (_state.value.cancelling) return
        _state.update { it.copy(cancelling = true) }
        viewModelScope.launch {
            when (val outcome = repo.cancelCheck(check.id)) {
                is ApiOutcome.Ok -> _state.update { it.copy(cancelling = false, check = outcome.value) }
                is ApiOutcome.Failed -> {
                    // Already paid/expired: the watcher's next tick shows the truth.
                    _state.update { it.copy(cancelling = false) }
                }
                is ApiOutcome.Offline -> _state.update { it.copy(cancelling = false, error = outcome.userMessage()) }
            }
        }
    }

    /** Back to the amount form for the next customer. */
    fun newRequest() {
        watcher?.cancel()
        checkKey.reset()
        _state.update { it.copy(check = null, amountText = "", description = "", error = null, secondsLeft = 0) }
    }

    private companion object {
        const val POLL_MS = 2_000L
    }
}
