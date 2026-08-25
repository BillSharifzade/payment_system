package tj.payment.wallet.ui.fx

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.Currency
import tj.payment.core.FxRateDto
import tj.payment.core.FxResponse
import tj.payment.core.Money
import tj.payment.core.WalletDto
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

data class FxUiState(
    val loading: Boolean = true,
    val wallets: List<WalletDto> = emptyList(),
    val rates: List<FxRateDto> = emptyList(),
    val fromId: String? = null,
    val toId: String? = null,
    val amountText: String = "",
    val converting: Boolean = false,
    val openingWallet: Boolean = false,
    val result: FxResponse? = null,
    val error: String? = null,
    val actionError: String? = null,
) {
    val from: WalletDto? get() = wallets.firstOrNull { it.id == fromId }
    val to: WalletDto? get() = wallets.firstOrNull { it.id == toId }

    /** Exchange needs two wallets in different currencies. */
    val needsSecondWallet: Boolean
        get() = !loading && error == null && wallets.map { it.currency }.distinct().size < 2

    val rate: FxRateDto?
        get() = rates.firstOrNull { it.base == from?.currency && it.quote == to?.currency }

    val amount: Money? get() = from?.let { Money.parse(amountText, Currency.of(it.currency)) }

    val insufficient: Boolean
        get() = amount?.let { a -> from?.let { a.minorUnits > it.balanceMinor } } == true

    /** The exact floored amount the backend will credit, or null. */
    val quoteMinor: Long?
        get() = rate?.let { r -> amount?.let { r.convert(it.minorUnits) } }

    val canConvert: Boolean
        get() = !converting && amount?.isPositive == true && !insufficient &&
            rate != null && (quoteMinor ?: 0) > 0
}

class FxViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(FxUiState())
    val state: StateFlow<FxUiState> = _state.asStateFlow()

    init {
        refresh()
    }

    fun refresh() {
        _state.value = _state.value.copy(loading = true, error = null, result = null)
        viewModelScope.launch {
            val walletsDeferred = async { repo.wallets() }
            val ratesDeferred = async { repo.fxRates() }
            val wallets = when (val o = walletsDeferred.await()) {
                is ApiOutcome.Ok -> o.value
                is ApiOutcome.Failed -> {
                    ratesDeferred.await()
                    _state.value = _state.value.copy(loading = false, error = o.userMessage())
                    return@launch
                }
                is ApiOutcome.Offline -> {
                    ratesDeferred.await()
                    _state.value = _state.value.copy(loading = false, error = OFFLINE_MESSAGE)
                    return@launch
                }
            }
            val rates = (ratesDeferred.await() as? ApiOutcome.Ok)?.value.orEmpty()

            // Default direction: primary (usually TJS) -> the first other-currency wallet.
            val from = _state.value.from ?: wallets.firstOrNull()
            val to = _state.value.to
                ?: wallets.firstOrNull { it.currency != from?.currency }
            _state.value = _state.value.copy(
                loading = false,
                wallets = wallets,
                rates = rates,
                fromId = from?.id,
                toId = to?.id,
            )
        }
    }

    /** One tap opens the missing second-currency wallet (USD for now). */
    fun openUsdWallet() {
        if (_state.value.openingWallet) return
        _state.value = _state.value.copy(openingWallet = true, actionError = null)
        viewModelScope.launch {
            when (val o = repo.createWallet("USD")) {
                is ApiOutcome.Ok -> {
                    _state.value = _state.value.copy(openingWallet = false)
                    refresh()
                }
                is ApiOutcome.Failed -> _state.value =
                    _state.value.copy(openingWallet = false, actionError = o.userMessage())
                is ApiOutcome.Offline -> _state.value =
                    _state.value.copy(openingWallet = false, actionError = OFFLINE_MESSAGE)
            }
        }
    }

    fun onAmountChange(value: String) {
        _state.value = _state.value.copy(amountText = value, actionError = null, result = null)
    }

    fun swap() {
        val s = _state.value
        _state.value = s.copy(fromId = s.toId, toId = s.fromId, amountText = "", result = null)
    }

    fun convert() {
        val s = _state.value
        val from = s.from ?: return
        val to = s.to ?: return
        val amount = s.amount ?: return
        if (!s.canConvert) return
        _state.value = s.copy(converting = true, actionError = null)
        viewModelScope.launch {
            when (val o = repo.convert(from.id, to.id, amount.minorUnits)) {
                is ApiOutcome.Ok -> {
                    _state.value = _state.value.copy(
                        converting = false,
                        result = o.value,
                        amountText = "",
                    )
                    refreshBalancesOnly()
                }
                is ApiOutcome.Failed -> _state.value =
                    _state.value.copy(converting = false, actionError = o.userMessage())
                is ApiOutcome.Offline -> _state.value =
                    _state.value.copy(converting = false, actionError = OFFLINE_MESSAGE)
            }
        }
    }

    private fun refreshBalancesOnly() {
        viewModelScope.launch {
            (repo.wallets() as? ApiOutcome.Ok)?.let {
                _state.value = _state.value.copy(wallets = it.value)
            }
        }
    }
}
