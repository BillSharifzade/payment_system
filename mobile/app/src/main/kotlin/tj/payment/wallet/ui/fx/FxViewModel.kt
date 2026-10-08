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
import tj.payment.core.PaymentKind
import tj.payment.core.PendingPaymentResolver
import tj.payment.core.SubmitResult
import tj.payment.core.WalletDto
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.NOT_STARTED_MESSAGE
import tj.payment.wallet.ui.userMessage

data class FxUiState(
    val loading: Boolean = true,
    val wallets: List<WalletDto> = emptyList(),
    val rates: List<FxRateDto> = emptyList(),
    val fromId: String? = null,
    val toId: String? = null,
    val amountText: String = "",
    /** A conversion is being approved or is in flight. */
    val converting: Boolean = false,
    val openingWallet: Boolean = false,
    /** The exact legs of a conversion that just went through. */
    val result: FxResponse? = null,
    /** A conversion confirmed as posted without its legs (resolved by a status check). */
    val exchanged: Money? = null,
    val error: String? = null,
    val actionError: String? = null,
    /**
     * The last attempt got no definitive answer (offline / 5xx). The conversion
     * is saved on the device with its key; the pending card below offers
     * "Finish" (same key — it can't convert twice) or "Discard" (voids the key).
     */
    val outcomeUnknown: Boolean = false,
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

/**
 * FX moves money between the user's own wallets — through the same
 * [tj.payment.core.PaymentSubmitter] as every money move: device approval, the
 * key persisted before the request, same-key retry across process death, void
 * before discard. (It used to keep its key in memory only, so a process kill or
 * re-entering the screen after an unknown outcome could convert twice.)
 */
class FxViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(FxUiState())
    val state: StateFlow<FxUiState> = _state.asStateFlow()

    /** This user's unsettled payment (an FX from a killed process, or a transfer), with Finish / Discard. */
    val pending = PendingPaymentResolver(repo.submitter, viewModelScope, onResolved = { refreshBalancesOnly() })

    init {
        refresh()
        pending.load()
    }

    fun refresh() {
        _state.value = _state.value.copy(loading = true, error = null, result = null, exchanged = null)
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
                    _state.value = _state.value.copy(loading = false, error = o.userMessage())
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
                    _state.value.copy(openingWallet = false, actionError = o.userMessage())
            }
        }
    }

    fun onAmountChange(value: String) {
        _state.value = _state.value.copy(
            amountText = value,
            actionError = null,
            result = null,
            exchanged = null,
            outcomeUnknown = false,
        )
    }

    fun swap() {
        val s = _state.value
        _state.value = s.copy(
            fromId = s.toId,
            toId = s.fromId,
            amountText = "",
            result = null,
            exchanged = null,
            actionError = null,
            outcomeUnknown = false,
        )
    }

    fun convert() {
        val s = _state.value
        val from = s.from ?: return
        val to = s.to ?: return
        val amount = s.amount ?: return
        if (!s.canConvert) return

        _state.value = s.copy(converting = true, actionError = null, result = null, exchanged = null, outcomeUnknown = false)
        viewModelScope.launch {
            val result = repo.submitter.submitNew(
                fromAccount = from.id,
                toAccount = to.id,
                amountMinor = amount.minorUnits,
                currency = from.currency,
                recipientLabel = to.currency,
                kind = PaymentKind.FX,
            )
            when (result) {
                is SubmitResult.Posted -> {
                    _state.value = _state.value.copy(
                        converting = false,
                        result = result.fx,
                        exchanged = if (result.fx == null) amount else null,
                        amountText = "",
                    )
                    refreshBalancesOnly()
                }
                // A definitive refusal: this exact request will never post.
                is SubmitResult.Rejected -> _state.value =
                    _state.value.copy(converting = false, actionError = result.code.userMessage())
                // The server may have converted. The key is persisted; the
                // pending card offers the same-key retry or a void.
                is SubmitResult.Unsettled -> settleUnknown()
                SubmitResult.NotStarted -> _state.value =
                    _state.value.copy(converting = false, actionError = NOT_STARTED_MESSAGE)
                // Not approved on the device: nothing stored, nothing sent.
                is SubmitResult.NotAuthorized -> _state.value =
                    _state.value.copy(converting = false, actionError = result.denial?.userMessage())
                is SubmitResult.Blocked -> {
                    _state.value = _state.value.copy(
                        converting = false,
                        actionError = "Finish or discard your unconfirmed payment first.",
                    )
                    pending.load(check = false)
                }
            }
        }
    }

    /**
     * Unknown outcome: force a balance fetch so the user judges against real
     * numbers, and surface the saved conversion in the pending card.
     */
    private suspend fun settleUnknown() {
        val refreshed = repo.refreshWallets()
        val wallets = (refreshed as? ApiOutcome.Ok)?.value ?: _state.value.wallets
        _state.value = _state.value.copy(
            converting = false,
            wallets = wallets,
            outcomeUnknown = true,
            actionError = "We couldn't confirm whether the exchange went through. It's saved on this " +
                "phone: finish it below — it can't convert twice — or discard it.",
        )
        pending.load(check = false)
    }

    private fun refreshBalancesOnly() {
        viewModelScope.launch {
            (repo.wallets() as? ApiOutcome.Ok)?.let {
                _state.value = _state.value.copy(wallets = it.value)
            }
        }
    }
}
