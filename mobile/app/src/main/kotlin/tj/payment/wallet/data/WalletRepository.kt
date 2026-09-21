package tj.payment.wallet.data

import java.util.UUID
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import tj.payment.core.AccountResponse
import tj.payment.core.ApiOutcome
import tj.payment.core.ClientConfigResponse
import tj.payment.core.DocumentResponse
import tj.payment.core.FxRateDto
import tj.payment.core.FxRequest
import tj.payment.core.FxResponse
import tj.payment.core.KycStatusResponse
import tj.payment.core.KycSubmissionDto
import tj.payment.core.PaymentSubmitter
import tj.payment.core.PendingPaymentStore
import tj.payment.core.ResolveResponse
import tj.payment.core.StatementResponse
import tj.payment.core.SubmitKycRequest
import tj.payment.core.TransferRequest
import tj.payment.core.WalletDto

/**
 * Everything money: wallets, statement, recipient lookup, and the send/convert
 * paths. Sends go ONLY through [submitter] — no screen may call the transfer
 * endpoint directly, so the persisted-idempotency-key rules can't be bypassed.
 *
 * Wallets and `/v1/config` are cached in memory and shared by Home, Send and
 * FX (which open seconds apart); every money move marks the wallets stale so
 * the next reader fetches.
 */
class WalletRepository(
    private val api: ApiClient,
    pendingStore: PendingPaymentStore,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    val submitter = PaymentSubmitter(
        store = pendingStore,
        newKey = { UUID.randomUUID().toString() },
        transfer = { p ->
            api.transfer(
                TransferRequest(p.fromAccount, p.toAccount, p.amountMinor, p.currency),
                idempotencyKey = p.idempotencyKey,
            ).also { markWalletsStale() } // any outcome but a refusal may have moved money
        },
        findPosted = { p -> findPostedByKey(p.fromAccount, p.idempotencyKey) },
    )

    private val _wallets = MutableStateFlow<List<WalletDto>?>(null)

    /** Last known wallets (null = never loaded this session). Updated by every fetch. */
    val wallets: StateFlow<List<WalletDto>?> = _wallets.asStateFlow()

    @Volatile private var walletsFetchedAtMs = 0L

    @Volatile private var walletsStale = true

    @Volatile private var configCache: Pair<ClientConfigResponse, Long>? = null

    /** Always the network; updates [wallets] on success. Home's refresh path. */
    suspend fun refreshWallets(): ApiOutcome<List<WalletDto>> =
        api.wallets().also {
            if (it is ApiOutcome.Ok) {
                _wallets.value = it.value
                walletsFetchedAtMs = clock()
                walletsStale = false
            }
        }

    /** The cache while it is fresh (recent and no money moved since), else the network. */
    suspend fun wallets(): ApiOutcome<List<WalletDto>> {
        val cached = _wallets.value
        if (cached != null && !walletsStale && clock() - walletsFetchedAtMs < WALLETS_FRESH_MS) {
            return ApiOutcome.Ok(cached)
        }
        return refreshWallets()
    }

    /** Call after anything that may have changed a balance. */
    fun markWalletsStale() {
        walletsStale = true
    }

    /** True when the next Home visit should not trust its last render. */
    val walletsAreStale: Boolean get() = walletsStale

    /** Forget everything (sign-out): the next user must never see this one's balances. */
    fun clearCache() {
        _wallets.value = null
        walletsStale = true
        configCache = null
    }

    suspend fun createWallet(currency: String): ApiOutcome<AccountResponse> =
        api.createWallet(currency).also { markWalletsStale() }

    /** Server pricing facts; cached for [CONFIG_FRESH_MS], stale copy on a failed refetch. */
    suspend fun config(): ApiOutcome<ClientConfigResponse> {
        val cached = configCache
        if (cached != null && clock() - cached.second < CONFIG_FRESH_MS) return ApiOutcome.Ok(cached.first)
        val fetched = api.config()
        return when {
            fetched is ApiOutcome.Ok -> fetched.also { configCache = it.value to clock() }
            cached != null -> ApiOutcome.Ok(cached.first)
            else -> fetched
        }
    }

    suspend fun statement(accountId: String, cursor: String?, limit: Int = 30): ApiOutcome<StatementResponse> =
        api.statement(accountId, cursor, limit)

    /**
     * Second opinion for the submitter: is a transfer carrying [idempotencyKey]
     * already in [fromAccount]'s statement? The backend uses the key as the
     * transaction id. A pending payment is recent, so the newest two pages are
     * enough. `Ok(false)` = not there; Failed/Offline = could not tell.
     */
    suspend fun findPostedByKey(fromAccount: String, idempotencyKey: String): ApiOutcome<Boolean> {
        var cursor: String? = null
        repeat(RECONCILE_PAGES) {
            when (val page = api.statement(fromAccount, cursor, limit = RECONCILE_PAGE_SIZE)) {
                is ApiOutcome.Ok -> {
                    if (page.value.entries.any { it.transactionId == idempotencyKey }) return ApiOutcome.Ok(true)
                    cursor = page.value.nextCursor ?: return ApiOutcome.Ok(false)
                }
                is ApiOutcome.Failed -> return page
                is ApiOutcome.Offline -> return page
            }
        }
        return ApiOutcome.Ok(false)
    }

    suspend fun resolveByPhone(phone: String): ApiOutcome<ResolveResponse> =
        api.resolveByPhone(phone)

    suspend fun kycStatus(): ApiOutcome<KycStatusResponse> = api.kycStatus()

    suspend fun uploadKycDocument(bytes: ByteArray, mimeType: String): ApiOutcome<DocumentResponse> =
        api.uploadKycDocument(bytes, mimeType)

    suspend fun submitKyc(fullName: String, documentType: String, documentRef: String): ApiOutcome<KycSubmissionDto> =
        api.submitKyc(
            SubmitKycRequest(
                requestedLevel = 1,
                fullName = fullName,
                documentType = documentType,
                documentRef = documentRef,
            ),
        )

    suspend fun fxRates(): ApiOutcome<List<FxRateDto>> = api.fxRates()

    /**
     * FX moves money between the caller's own wallets. The caller (FxViewModel)
     * owns [idempotencyKey] and keeps it for the SAME (from, to, amount) attempt
     * until a definitive outcome, so an offline/5xx retry can never convert twice.
     */
    suspend fun convert(
        fromAccount: String,
        toAccount: String,
        amountMinor: Long,
        idempotencyKey: String,
    ): ApiOutcome<FxResponse> =
        api.fx(FxRequest(fromAccount, toAccount, amountMinor), idempotencyKey = idempotencyKey)
            .also { markWalletsStale() }

    private companion object {
        const val WALLETS_FRESH_MS = 60_000L
        const val CONFIG_FRESH_MS = 10 * 60_000L
        const val RECONCILE_PAGES = 2
        const val RECONCILE_PAGE_SIZE = 50
    }
}
