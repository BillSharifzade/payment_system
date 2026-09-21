package tj.payment.wallet.ui.history

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.ApiOutcome
import tj.payment.core.StatementEntryDto
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.ui.OFFLINE_MESSAGE
import tj.payment.wallet.ui.userMessage

data class HistoryUiState(
    val entries: List<StatementEntryDto> = emptyList(),
    /** First page loading (blank screen) vs. appending an older page. */
    val loading: Boolean = true,
    val loadingMore: Boolean = false,
    val error: String? = null,
    /** The last "older page" fetch failed; a retry row is shown at the tail. */
    val loadMoreError: String? = null,
    /** null after the last page — nothing older to fetch. */
    val nextCursor: String? = null,
    val endReached: Boolean = false,
)

class HistoryViewModel(
    private val repo: WalletRepository,
    private val accountId: String,
) : ViewModel() {

    private val _state = MutableStateFlow(HistoryUiState())
    val state: StateFlow<HistoryUiState> = _state.asStateFlow()

    init {
        refresh()
    }

    fun refresh() {
        _state.value = HistoryUiState(loading = true)
        viewModelScope.launch {
            when (val outcome = repo.statement(accountId, cursor = null)) {
                is ApiOutcome.Ok -> _state.value = HistoryUiState(
                    entries = outcome.value.entries,
                    loading = false,
                    nextCursor = outcome.value.nextCursor,
                    endReached = outcome.value.nextCursor == null,
                )
                is ApiOutcome.Failed -> _state.value =
                    HistoryUiState(loading = false, error = outcome.userMessage())
                is ApiOutcome.Offline -> _state.value =
                    HistoryUiState(loading = false, error = OFFLINE_MESSAGE)
            }
        }
    }

    /**
     * Fetch the next (older) page; keyset cursor makes this gap-free. After a
     * failure the scroll trigger stays quiet (no retry storm) until the user
     * taps the retry row ([retryLoadMore]).
     */
    fun loadMore() {
        val s = _state.value
        val cursor = s.nextCursor ?: return
        if (s.loadingMore || s.loading || s.loadMoreError != null) return
        _state.value = s.copy(loadingMore = true)
        viewModelScope.launch {
            when (val outcome = repo.statement(accountId, cursor = cursor)) {
                is ApiOutcome.Ok -> _state.value = _state.value.copy(
                    entries = _state.value.entries + outcome.value.entries,
                    loadingMore = false,
                    nextCursor = outcome.value.nextCursor,
                    endReached = outcome.value.nextCursor == null,
                )
                // A failed page-load isn't fatal — keep what we have, show a
                // visible retry row instead of silently stopping the list.
                is ApiOutcome.Failed -> _state.value = _state.value.copy(
                    loadingMore = false,
                    loadMoreError = outcome.userMessage(),
                )
                is ApiOutcome.Offline -> _state.value = _state.value.copy(
                    loadingMore = false,
                    loadMoreError = OFFLINE_MESSAGE,
                )
            }
        }
    }

    fun retryLoadMore() {
        _state.value = _state.value.copy(loadMoreError = null)
        loadMore()
    }
}
