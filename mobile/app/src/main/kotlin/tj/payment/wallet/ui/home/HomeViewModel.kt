package tj.payment.wallet.ui.home

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
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
    /** One-time: an earlier payment record on this device could not be read. */
    val unreadableRecordNotice: Boolean = false,
) {
    val primaryWallet: WalletDto? get() = wallets.firstOrNull()
}

class HomeViewModel(
    private val auth: AuthRepository,
    private val repo: WalletRepository,
    consumeUnreadableRecordNotice: () -> Boolean = { false },
    private val clock: () -> Long = System::currentTimeMillis,
) : ViewModel() {

    private val _state = MutableStateFlow(
        HomeUiState(
            // Cached balances paint at once; the skeleton only shows on a cold session.
            wallets = repo.wallets.value.orEmpty(),
            loading = repo.wallets.value == null,
            unreadableRecordNotice = consumeUnreadableRecordNotice(),
        ),
    )
    val state: StateFlow<HomeUiState> = _state.asStateFlow()

    private var refreshJob: Job? = null
    private var lastSuccessAtMs = 0L

    init {
        // Balances are shared with Send/FX: whoever fetched last, Home shows it.
        viewModelScope.launch {
            repo.wallets.collect { wallets ->
                if (wallets != null) _state.update { it.copy(wallets = wallets) }
            }
        }
    }

    /**
     * Called on every resume (first entry, back from Send/KYC/FX, back from the
     * background). Refreshes when money moved since the last render, when the
     * last attempt failed, or when the render is older than [MIN_REFRESH_INTERVAL_MS];
     * otherwise the cached render stands — no wave of requests per screen flip.
     */
    fun refreshIfDue() {
        val s = _state.value
        val due = repo.walletsAreStale || s.error != null || lastSuccessAtMs == 0L ||
            clock() - lastSuccessAtMs >= MIN_REFRESH_INTERVAL_MS
        if (due) refresh()
    }

    fun refresh() {
        // A newer refresh supersedes an in-flight one, so a slow old answer can
        // never overwrite a fresh one.
        refreshJob?.cancel()
        _state.update { it.copy(loading = true, error = null) }
        refreshJob = viewModelScope.launch {
            // Wallets and KYC in parallel; the statement needs the wallet id.
            val walletsDeferred = async { repo.refreshWallets() }
            val kycDeferred = async { repo.kycStatus() }

            val wallets = when (val outcome = walletsDeferred.await()) {
                // Registration auto-creates a TJS wallet; if this account somehow
                // predates that (or the auto-create failed), heal it here.
                is ApiOutcome.Ok -> outcome.value.ifEmpty {
                    repo.createWallet("TJS")
                    (repo.refreshWallets() as? ApiOutcome.Ok)?.value.orEmpty()
                }
                is ApiOutcome.Failed -> {
                    _state.update { it.copy(loading = false, error = outcome.userMessage()) }
                    kycDeferred.await()
                    return@launch
                }
                is ApiOutcome.Offline -> {
                    _state.update { it.copy(loading = false, error = OFFLINE_MESSAGE) }
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

            lastSuccessAtMs = clock()
            _state.update {
                it.copy(
                    loading = false,
                    wallets = wallets,
                    kycLevel = kycLevel,
                    kycPending = kycPending,
                    recent = recent,
                    error = null,
                )
            }
        }
    }

    fun dismissUnreadableRecordNotice() {
        _state.update { it.copy(unreadableRecordNotice = false) }
    }

    /** Navigation follows from SecureSession.signedOut, observed in AppRoot. */
    fun logout() {
        viewModelScope.launch { auth.logout() }
    }

    private companion object {
        const val MIN_REFRESH_INTERVAL_MS = 15_000L
    }
}
