package tj.payment.wallet.ui.fx

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import java.util.UUID
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
    /** A conversion is in flight, or its unknown outcome is being checked. */
    val converting: Boolean = false,
    val openingWallet: Boolean = false,
    val result: FxResponse? = null,
    val error: String? = null,
    val actionError: String? = null,
    /**
     * The last attempt did not get a definitive answer (offline / 5xx). The
     * balances shown have been re-fetched; tapping Exchange again resends the
     * SAME request (same idempotency key), so it cannot convert twice.
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

class FxViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(FxUiState())
    val state: StateFlow<FxUiState> = _state.asStateFlow()

    /**
     * One idempotency key per (from, to, amount) attempt, kept until the server
     * gives a definitive answer. An Offline / 5xx / unreadable outcome keeps it,
     * so the next tap on Exchange replays the SAME request and the backend
     * converts at most once. A definitive outcome (posted or refused) drops it;
     * a different (from, to, amount) is a different intent and mints a new one.
     *
     * Residual (documented, accepted): unlike a transfer, this key lives only in
     * the ViewModel — it is not persisted, so it does not survive process death
     * mid-request. FX moves money between the user's OWN wallets, so the worst
     * case of a lost key is a second conversion the user can see on Home and
     * reverse, not money gone to someone else. Transfers to other people take the
     * persisted PaymentSubmitter path instead. The forced balance refresh after an
     * unknown outcome is the user's cue to look before tapping again.
     */
    private var attempt: FxAttempt? = null

    private data class FxAttempt(val fromId: String, val toId: String, val amountMinor: Long, val key: String)

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
        _state.value = _state.value.copy(amountText = value, actionError = null, result = null, outcomeUnknown = false)
    }

    fun swap() {
        val s = _state.value
        _state.value = s.copy(fromId = s.toId, toId = s.fromId, amountText = "", result = null, actionError = null, outcomeUnknown = false)
    }

    fun convert() {
        val s = _state.value
        val from = s.from ?: return
        val to = s.to ?: return
        val amount = s.amount ?: return
        if (!s.canConvert) return

        // Reuse the key of an unsettled attempt for the SAME intent; otherwise mint.
        val current = attempt
            ?.takeIf { it.fromId == from.id && it.toId == to.id && it.amountMinor == amount.minorUnits }
            ?: FxAttempt(from.id, to.id, amount.minorUnits, UUID.randomUUID().toString()).also { attempt = it }

        _state.value = s.copy(converting = true, actionError = null, result = null, outcomeUnknown = false)
        viewModelScope.launch {
            when (val o = repo.convert(from.id, to.id, amount.minorUnits, idempotencyKey = current.key)) {
                is ApiOutcome.Ok -> {
                    attempt = null
                    _state.value = _state.value.copy(
                        converting = false,
                        result = o.value,
                        amountText = "",
                    )
                    refreshBalancesOnly()
                }
                is ApiOutcome.Failed -> if (o.undetermined) {
                    // 5xx / retry_later / timeout / 429 / unreadable 2xx: the
                    // server may have converted. Keep the key.
                    settleUnknown()
                } else {
                    // A definitive refusal: this exact request will never post.
                    attempt = null
                    _state.value = _state.value.copy(converting = false, actionError = o.userMessage())
                }
                is ApiOutcome.Offline -> settleUnknown()
            }
        }
    }

    /**
     * Unknown outcome: keep the key, and force a balance fetch BEFORE Exchange
     * is re-enabled so the user judges the next tap against real numbers, not
     * the pre-attempt ones.
     */
    private suspend fun settleUnknown() {
        val refreshed = repo.refreshWallets()
        val wallets = (refreshed as? ApiOutcome.Ok)?.value ?: _state.value.wallets
        _state.value = _state.value.copy(
            converting = false,
            wallets = wallets,
            outcomeUnknown = true,
            actionError = if (refreshed is ApiOutcome.Ok) {
                "We couldn't confirm whether the exchange went through. Your balances were " +
                    "just refreshed — check them above. Tapping Exchange again resends the same " +
                    "request, so it can't convert twice."
            } else {
                "We couldn't confirm whether the exchange went through, and couldn't refresh " +
                    "your balances either. Check them on Home before trying again — retrying " +
                    "resends the same request, so it can't convert twice."
            },
        )
    }

    private fun refreshBalancesOnly() {
        viewModelScope.launch {
            (repo.wallets() as? ApiOutcome.Ok)?.let {
                _state.value = _state.value.copy(wallets = it.value)
            }
        }
    }
}
