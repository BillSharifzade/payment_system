package tj.payment.wallet.ui.home

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.WalletDto
import tj.payment.wallet.data.AuthRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

data class HomeUiState(
    val loading: Boolean = true,
    val wallets: List<WalletDto> = emptyList(),
    val error: String? = null,
    val loggedOut: Boolean = false,
)

class HomeViewModel(private val repo: AuthRepository) : ViewModel() {

    private val _state = MutableStateFlow(HomeUiState())
    val state: StateFlow<HomeUiState> = _state.asStateFlow()

    init {
        refresh()
    }

    fun refresh() {
        _state.value = _state.value.copy(loading = true, error = null)
        viewModelScope.launch {
            _state.value = when (val outcome = repo.wallets()) {
                is ApiOutcome.Ok -> _state.value.copy(loading = false, wallets = outcome.value, error = null)
                is ApiOutcome.Failed -> _state.value.copy(loading = false, error = outcome.userMessage())
                is ApiOutcome.Offline -> _state.value.copy(loading = false, error = OFFLINE_MESSAGE)
            }
        }
    }

    fun logout() {
        viewModelScope.launch {
            repo.logout()
            _state.value = _state.value.copy(loggedOut = true)
        }
    }
}
