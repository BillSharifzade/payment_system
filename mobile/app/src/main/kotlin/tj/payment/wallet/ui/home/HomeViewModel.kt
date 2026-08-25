package tj.payment.wallet.ui.home

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.StatementEntryDto
import tj.payment.core.WalletDto
import tj.payment.wallet.data.AuthRepository
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

data class HomeUiState(
    val loading: Boolean = true,
    val wallets: List<WalletDto> = emptyList(),
    /** Level 0 shows the "verify your identity" banner; sends need level 1. */
    val kycLevel: Int? = null,
    val kycPending: Boolean = false,
    /** First few movements of the primary wallet — the at-a-glance activity. */
    val recent: List<StatementEntryDto> = emptyList(),
    val error: String? = null,
    val loggedOut: Boolean = false,
) {
    val primaryWallet: WalletDto? get() = wallets.firstOrNull()
}

class HomeViewModel(
    private val auth: AuthRepository,
    private val repo: WalletRepository,
) : ViewModel() {

    private val _state = MutableStateFlow(HomeUiState())
    val state: StateFlow<HomeUiState> = _state.asStateFlow()

    // No init{refresh()}: the screen triggers a refresh on every (re)entry, so
    // balances are fresh after returning from Send/KYC/FX without a double
    // fetch on first composition.

    fun refresh() {
        _state.value = _state.value.copy(loading = true, error = null)
        viewModelScope.launch {
            // Wallets and KYC in parallel; the statement needs the wallet id.
            val walletsDeferred = async { repo.wallets() }
            val kycDeferred = async { repo.kycStatus() }

            val wallets = when (val outcome = walletsDeferred.await()) {
                // Registration auto-creates a TJS wallet; if this account somehow
                // predates that (or the auto-create failed), heal it here.
                is ApiOutcome.Ok -> outcome.value.ifEmpty {
                    repo.createWallet("TJS")
                    (repo.wallets() as? ApiOutcome.Ok)?.value.orEmpty()
                }
                is ApiOutcome.Failed -> {
                    _state.value = _state.value.copy(loading = false, error = outcome.userMessage())
                    kycDeferred.await()
                    return@launch
                }
                is ApiOutcome.Offline -> {
                    _state.value = _state.value.copy(loading = false, error = OFFLINE_MESSAGE)
                    kycDeferred.await()
                    return@launch
                }
            }

            val (kycLevel, kycPending) = when (val outcome = kycDeferred.await()) {
                is ApiOutcome.Ok -> outcome.value.kycLevel to
                    (outcome.value.latestSubmission?.status == "pending")
                else -> null to false // KYC banner is best-effort; never block Home on it
            }

            val recent = wallets.firstOrNull()?.let { primary ->
                when (val outcome = repo.statement(primary.id, cursor = null, limit = 5)) {
                    is ApiOutcome.Ok -> outcome.value.entries
                    else -> emptyList()
                }
            }.orEmpty()

            _state.value = _state.value.copy(
                loading = false,
                wallets = wallets,
                kycLevel = kycLevel,
                kycPending = kycPending,
                recent = recent,
                error = null,
            )
        }
    }

    fun logout() {
        viewModelScope.launch {
            auth.logout()
            _state.value = _state.value.copy(loggedOut = true)
        }
    }
}
