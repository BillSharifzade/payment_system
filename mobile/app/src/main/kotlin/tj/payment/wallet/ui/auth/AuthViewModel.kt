package tj.payment.wallet.ui.auth

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.wallet.data.AuthRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

enum class AuthMode { LOGIN, REGISTER }

data class AuthUiState(
    val mode: AuthMode = AuthMode.LOGIN,
    val phone: String = "",
    val password: String = "",
    val submitting: Boolean = false,
    val error: String? = null,
    val authenticated: Boolean = false,
) {
    // Digits-only, plausible length — mirrors the backend's normalize_phone.
    val phoneDigits: String get() = phone.filter { it.isDigit() }
    val canSubmit: Boolean
        get() = !submitting && phoneDigits.length in 7..15 && password.length >= 8
}

class AuthViewModel(private val repo: AuthRepository) : ViewModel() {

    private val _state = MutableStateFlow(AuthUiState())
    val state: StateFlow<AuthUiState> = _state.asStateFlow()

    fun onPhoneChange(value: String) {
        _state.value = _state.value.copy(phone = value, error = null)
    }

    fun onPasswordChange(value: String) {
        _state.value = _state.value.copy(password = value, error = null)
    }

    fun toggleMode() {
        val next = if (_state.value.mode == AuthMode.LOGIN) AuthMode.REGISTER else AuthMode.LOGIN
        _state.value = _state.value.copy(mode = next, error = null)
    }

    fun submit() {
        val s = _state.value
        if (!s.canSubmit) return
        _state.value = s.copy(submitting = true, error = null)
        viewModelScope.launch {
            val outcome = when (s.mode) {
                AuthMode.LOGIN -> repo.login(s.phoneDigits, s.password)
                AuthMode.REGISTER -> repo.register(s.phoneDigits, s.password)
            }
            _state.value = when (outcome) {
                is ApiOutcome.Ok -> _state.value.copy(submitting = false, authenticated = true)
                is ApiOutcome.Failed -> _state.value.copy(submitting = false, error = outcome.userMessage())
                is ApiOutcome.Offline -> _state.value.copy(submitting = false, error = OFFLINE_MESSAGE)
            }
        }
    }
}
