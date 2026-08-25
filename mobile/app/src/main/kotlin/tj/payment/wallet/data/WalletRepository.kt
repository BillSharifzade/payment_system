package tj.payment.wallet.data

import java.util.UUID
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
 */
class WalletRepository(
    private val api: ApiClient,
    pendingStore: PendingPaymentStore,
) {
    val submitter = PaymentSubmitter(
        store = pendingStore,
        newKey = { UUID.randomUUID().toString() },
        transfer = { p ->
            api.transfer(
                TransferRequest(p.fromAccount, p.toAccount, p.amountMinor, p.currency),
                idempotencyKey = p.idempotencyKey,
            )
        },
    )

    suspend fun wallets(): ApiOutcome<List<WalletDto>> = api.wallets()

    suspend fun createWallet(currency: String): ApiOutcome<AccountResponse> =
        api.createWallet(currency)

    suspend fun config(): ApiOutcome<ClientConfigResponse> = api.config()

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

    /**
     * FX conversions move money between the caller's own wallets, so a lost
     * outcome can't pay the wrong person — a fresh key per user confirmation is
     * the right contract here (the server still dedupes retries of this key).
     */
    suspend fun convert(fromAccount: String, toAccount: String, amountMinor: Long): ApiOutcome<FxResponse> =
        api.fx(
            FxRequest(fromAccount, toAccount, amountMinor),
            idempotencyKey = UUID.randomUUID().toString(),
        )
}
