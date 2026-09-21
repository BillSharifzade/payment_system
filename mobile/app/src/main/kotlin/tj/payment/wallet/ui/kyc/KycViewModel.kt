package tj.payment.wallet.ui.kyc

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

data class KycUiState(
    val loading: Boolean = true,
    val kycLevel: Int? = null,
    /** "pending" | "approved" | "rejected" | null (never submitted). */
    val submissionStatus: String? = null,
    val error: String? = null,

    // The submission form (shown at level 0 with no pending submission).
    val fullName: String = "",
    val documentType: String = "passport",
    val uploading: Boolean = false,
    val documentRef: String? = null,
    val submitting: Boolean = false,
    val formError: String? = null,
) {
    val verified: Boolean get() = (kycLevel ?: 0) >= 1
    val underReview: Boolean get() = submissionStatus == "pending"
    val showForm: Boolean get() = !loading && !verified && !underReview && error == null
    val canSubmit: Boolean
        get() = fullName.trim().length >= 3 && documentRef != null && !submitting && !uploading
}

class KycViewModel(private val repo: WalletRepository) : ViewModel() {

    private val _state = MutableStateFlow(KycUiState())
    val state: StateFlow<KycUiState> = _state.asStateFlow()

    init {
        refresh()
    }

    fun refresh(silent: Boolean = false) {
        if (!silent) _state.value = _state.value.copy(loading = true, error = null)
        viewModelScope.launch {
            when (val outcome = repo.kycStatus()) {
                is ApiOutcome.Ok -> _state.value = _state.value.copy(
                    loading = false,
                    kycLevel = outcome.value.kycLevel,
                    submissionStatus = outcome.value.latestSubmission?.status,
                    error = null,
                )
                is ApiOutcome.Failed -> _state.value = _state.value.copy(
                    loading = false,
                    error = if (silent) _state.value.error else outcome.userMessage(),
                )
                is ApiOutcome.Offline -> _state.value = _state.value.copy(
                    loading = false,
                    error = if (silent) _state.value.error else OFFLINE_MESSAGE,
                )
            }
        }
    }

    fun onNameChange(value: String) {
        _state.value = _state.value.copy(fullName = value, formError = null)
    }

    fun onDocumentType(value: String) {
        _state.value = _state.value.copy(documentType = value)
    }

    /**
     * The picker refused the file before reading it (too large, unreadable).
     * Surfaced like any other form error so the user can pick another one.
     */
    fun onDocumentRejected(reason: String) {
        _state.value = _state.value.copy(formError = reason)
    }

    /** Called with the picked file's bytes; the picker already enforced the cap,
     * this is the last line in case a caller bypasses it. */
    fun uploadDocument(bytes: ByteArray, mimeType: String) {
        if (bytes.size > MAX_DOCUMENT_BYTES) {
            _state.value = _state.value.copy(formError = DOCUMENT_TOO_LARGE)
            return
        }
        _state.value = _state.value.copy(uploading = true, formError = null)
        viewModelScope.launch {
            when (val outcome = repo.uploadKycDocument(bytes, mimeType)) {
                is ApiOutcome.Ok -> _state.value = _state.value.copy(
                    uploading = false,
                    documentRef = outcome.value.documentRef,
                )
                is ApiOutcome.Failed -> _state.value = _state.value.copy(
                    uploading = false,
                    formError = outcome.userMessage(),
                )
                is ApiOutcome.Offline -> _state.value = _state.value.copy(
                    uploading = false,
                    formError = OFFLINE_MESSAGE,
                )
            }
        }
    }

    fun submit() {
        val s = _state.value
        val ref = s.documentRef ?: return
        if (!s.canSubmit) return
        _state.value = s.copy(submitting = true, formError = null)
        viewModelScope.launch {
            when (val outcome = repo.submitKyc(s.fullName.trim(), s.documentType, ref)) {
                is ApiOutcome.Ok -> _state.value = _state.value.copy(
                    submitting = false,
                    submissionStatus = outcome.value.status,
                )
                is ApiOutcome.Failed -> _state.value = _state.value.copy(
                    submitting = false,
                    formError = outcome.userMessage(),
                )
                is ApiOutcome.Offline -> _state.value = _state.value.copy(
                    submitting = false,
                    formError = OFFLINE_MESSAGE,
                )
            }
        }
    }

    companion object {
        /** The server's upload cap; checked BEFORE the file is read into memory. */
        const val MAX_DOCUMENT_BYTES = 5 * 1024 * 1024
        const val DOCUMENT_TOO_LARGE = "The file is over 5 MB. Pick a smaller one."
    }
}
