package tj.payment.wallet.data

import java.util.UUID
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import tj.payment.core.AccountResponse
import tj.payment.core.ApiOutcome
import tj.payment.core.CheckDto
import tj.payment.core.ClientConfigResponse
import tj.payment.core.CreateCheckRequest
import tj.payment.core.DocumentResponse
import tj.payment.core.ErrorCode
import tj.payment.core.FxRateDto
import tj.payment.core.FxRequest
import tj.payment.core.KycStatusResponse
import tj.payment.core.KycSubmissionDto
import tj.payment.core.PayCheckRequest
import tj.payment.core.PaymentAuthorizer
import tj.payment.core.PaymentKind
import tj.payment.core.PaymentSubmitter
import tj.payment.core.PendingPayment
import tj.payment.core.PendingPaymentStore
import tj.payment.core.ResolveResponse
import tj.payment.core.Settlement
import tj.payment.core.StatementResponse
import tj.payment.core.SubmitKycRequest
import tj.payment.core.TransferRequest
import tj.payment.core.WalletDto
import tj.payment.core.deviceHeaders
import tj.payment.core.map

/**
 * Everything money: wallets, statement, recipient lookup, and the send /
 * pay-check / convert paths. Every money move goes ONLY through [submitter] —
 * no screen may call a money endpoint directly, so the device-approval and
 * persisted-idempotency-key rules can't be bypassed.
 *
 * Wallets and `/v1/config` are cached in memory and shared by Home, Send and
 * FX (which open seconds apart); every money move marks the wallets stale so
 * the next reader fetches.
 *
 * @param currentUser The signed-in user id (pending payments are per user).
 * @param authorizer Strong device authentication, asked before every new money move.
 * @param onDeviceRejected The server refused the payment's device signature
 *   (unknown/revoked device): the registration must be renewed with the password.
 */
class WalletRepository(
    private val api: ApiClient,
    pendingStore: PendingPaymentStore,
    currentUser: () -> String?,
    authorizer: PaymentAuthorizer,
    private val clock: () -> Long = System::currentTimeMillis,
    private val onDeviceRejected: suspend (userId: String) -> Unit = {},
) {
    val submitter = PaymentSubmitter(
        store = pendingStore,
        currentUser = currentUser,
        newKey = { UUID.randomUUID().toString() },
        authorizer = authorizer,
        // Any outcome but a refusal may have moved money.
        execute = { p ->
            send(p).also { outcome ->
                markWalletsStale()
                if (outcome is ApiOutcome.Failed && outcome.coded && outcome.code in DEVICE_REFUSALS) onDeviceRejected(p.userId)
            }
        },
        lookup = { id -> api.transaction(id) },
        void = { id -> api.voidTransaction(id).also { markWalletsStale() } },
        clock = clock,
    )

    /** The one place a persisted payment becomes a request; the key is the transaction id for all three. */
    private suspend fun send(p: PendingPayment): ApiOutcome<Settlement> = when (p.kind) {
        PaymentKind.TRANSFER ->
            api.transfer(
                TransferRequest(p.fromAccount, p.toAccount, p.amountMinor, p.currency),
                idempotencyKey = p.idempotencyKey,
                deviceHeaders = p.deviceHeaders(),
            ).map { Settlement(it.transactionId, alreadyPosted = it.status == "already_posted") }
        // A scanned check: the server debits p.fromAccount and credits the merchant.
        PaymentKind.CHECK -> {
            val checkId = p.checkId
            if (checkId == null) {
                ApiOutcome.Failed(ErrorCode.BAD_REQUEST, 0, "check payment without a check id")
            } else {
                api.payCheck(
                    checkId,
                    PayCheckRequest(account = p.fromAccount),
                    idempotencyKey = p.idempotencyKey,
                    deviceHeaders = p.deviceHeaders(),
                ).map { Settlement(it.transactionId, alreadyPosted = it.status == "already_posted") }
            }
        }
        // Between the user's own wallets; the answer carries the exact legs.
        PaymentKind.FX ->
            api.fx(
                FxRequest(p.fromAccount, p.toAccount, p.amountMinor),
                idempotencyKey = p.idempotencyKey,
                deviceHeaders = p.deviceHeaders(),
            ).map { Settlement(it.transactionId, alreadyPosted = false, fx = it) }
    }

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

    /**
     * Forget everything (sign-out): the next user must never see this one's
     * balances or pending payment. The pending RECORD stays on disk, bound to
     * its owner, and resurfaces when they sign back in.
     */
    fun clearCache() {
        _wallets.value = null
        walletsStale = true
        configCache = null
        submitter.forget()
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

    // FX conversions go through [submitter] (kind = FX) like every money move:
    // persisted key, device approval, same-key retry, void before discard.

    // --- Checks (request money by QR / pay a scanned check) ---

    /**
     * Open a check on [account]. [idempotencyKey] is owned by the caller and IS
     * the check id: a retried create returns the same check, never a second one.
     */
    suspend fun createCheck(
        account: String,
        amountMinor: Long,
        currency: String,
        description: String?,
        idempotencyKey: String,
    ): ApiOutcome<CheckDto> =
        api.createCheck(CreateCheckRequest(account, amountMinor, currency, description), idempotencyKey)

    suspend fun check(id: String): ApiOutcome<CheckDto> = api.check(id)

    suspend fun cancelCheck(id: String): ApiOutcome<CheckDto> = api.cancelCheck(id)

    private companion object {
        val DEVICE_REFUSALS = setOf(ErrorCode.DEVICE_SIGNATURE_INVALID, ErrorCode.DEVICE_SIGNATURE_REQUIRED)
        const val WALLETS_FRESH_MS = 60_000L
        const val CONFIG_FRESH_MS = 10 * 60_000L
    }
}
