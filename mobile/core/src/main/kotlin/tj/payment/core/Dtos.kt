package tj.payment.core

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/**
 * Wire DTOs mirroring the backend exactly (verified against crates/api/src/lib.rs
 * and error.rs, 2026-07-20). Hand-written on purpose: the set is small and this
 * avoids an OpenAPI/codegen toolchain for the MVP. @SerialName keeps the Kotlin
 * side idiomatic camelCase while the wire stays snake_case.
 */

@Serializable
data class CredentialsRequest(
    val phone: String,
    val password: String,
)

@Serializable
data class RefreshRequest(
    @SerialName("refresh_token") val refreshToken: String,
)

@Serializable
data class TokenResponse(
    @SerialName("user_id") val userId: String,
    @SerialName("access_token") val accessToken: String,
    @SerialName("refresh_token") val refreshToken: String,
    @SerialName("token_type") val tokenType: String = "Bearer",
    // Seconds; the server owns this — the client must not hardcode 15 minutes.
    // Defaulted so a server that stops sending it cannot break login; 0 means
    // "unknown", which disables proactive refresh (the 401 path still works).
    @SerialName("expires_in") val expiresInSeconds: Long = 0,
)

@Serializable
data class WalletDto(
    val id: String,
    val currency: String,
    @SerialName("balance_minor") val balanceMinor: Long,
    val display: String,
) {
    fun money(): Money = Money.ofMinor(balanceMinor, Currency.of(currency))
}

/** Response of GET /v1/users/resolve — the "check number" / QR-scan lookup. */
@Serializable
data class ResolveResponse(
    @SerialName("wallet_id") val walletId: String,
    val currency: String,
    /** Verified full name, or null when the account has no approved KYC name. */
    val name: String? = null,
    @SerialName("name_verified") val nameVerified: Boolean = false,
)

/** Error envelope: {"error": {"code": "...", "message": "...", "request_id": "..."}}. */
@Serializable
data class ErrorEnvelope(val error: ErrorBody) {
    @Serializable
    data class ErrorBody(
        val code: String? = null,
        val message: String? = null,
        /** Server correlation id — quote it to support; never shown as the error itself. */
        @SerialName("request_id") val requestId: String? = null,
    )
}

@Serializable
data class CreateWalletRequest(val currency: String)

@Serializable
data class AccountResponse(
    val id: String,
    val currency: String,
)

/**
 * One row of GET /v1/accounts/{id}/transactions. The server classifies the
 * movement from this account's point of view ([kind]) and names the other user
 * for transfers, so History can say who — not just debit/credit.
 */
@Serializable
data class StatementEntryDto(
    @SerialName("entry_id") val entryId: String,
    @SerialName("transaction_id") val transactionId: String,
    /** "debit" (money out) or "credit" (money in). */
    val direction: String,
    @SerialName("amount_minor") val amountMinor: Long,
    val currency: String,
    @SerialName("created_at_ms") val createdAtMs: Long,
    /** "transfer" | "deposit" | "fx" | "fee" | "withdrawal" | "other". */
    val kind: String = "other",
    @SerialName("counterparty_phone") val counterpartyPhone: String? = null,
    @SerialName("counterparty_name") val counterpartyName: String? = null,
) {
    val isCredit: Boolean get() = direction == "credit"

    fun money(): Money = Money.ofMinor(amountMinor, Currency.of(currency))
}

@Serializable
data class StatementResponse(
    val entries: List<StatementEntryDto>,
    /** Pass back as `cursor` for the next (older) page; null on the last page. */
    @SerialName("next_cursor") val nextCursor: String? = null,
)

@Serializable
data class TransferRequest(
    @SerialName("from_account") val fromAccount: String,
    @SerialName("to_account") val toAccount: String,
    @SerialName("amount_minor") val amountMinor: Long,
    val currency: String,
)

/** Response of POST /v1/transfers — "posted", or "already_posted" on a replay. */
@Serializable
data class PostResponse(
    @SerialName("transaction_id") val transactionId: String,
    // Defaulted: a 201 that omits it still means "posted". Only "already_posted"
    // changes what the user sees.
    val status: String = "posted",
)

@Serializable
data class KycSubmissionDto(
    val id: String,
    /** "pending" | "approved" | "rejected". */
    val status: String,
    @SerialName("requested_level") val requestedLevel: Int,
)

@Serializable
data class KycStatusResponse(
    @SerialName("kyc_level") val kycLevel: Int,
    @SerialName("latest_submission") val latestSubmission: KycSubmissionDto? = null,
)

@Serializable
data class SubmitKycRequest(
    @SerialName("requested_level") val requestedLevel: Int,
    @SerialName("full_name") val fullName: String,
    @SerialName("document_type") val documentType: String,
    @SerialName("document_ref") val documentRef: String,
)

/** Response of POST /v1/kyc/documents (multipart upload). */
@Serializable
data class DocumentResponse(
    @SerialName("document_ref") val documentRef: String,
)

/**
 * One admin-set FX rate: `quote_minor = base_minor * rateNum / rateDen`,
 * floored — integer math end to end, mirroring the backend exactly.
 */
@Serializable
data class FxRateDto(
    val base: String,
    val quote: String,
    @SerialName("rate_num") val rateNum: Long,
    @SerialName("rate_den") val rateDen: Long,
    @SerialName("updated_at_ms") val updatedAtMs: Long,
) {
    /** The floored conversion the backend will apply, or null on overflow. */
    fun convert(baseMinor: Long): Long? = try {
        Math.multiplyExact(baseMinor, rateNum) / rateDen
    } catch (_: ArithmeticException) {
        null
    }
}

@Serializable
data class FxRequest(
    @SerialName("from_account") val fromAccount: String,
    @SerialName("to_account") val toAccount: String,
    @SerialName("amount_minor") val amountMinor: Long,
)

@Serializable
data class FxResponse(
    @SerialName("transaction_id") val transactionId: String,
    @SerialName("debited_minor") val debitedMinor: Long,
    @SerialName("credited_minor") val creditedMinor: Long,
    @SerialName("from_currency") val fromCurrency: String,
    @SerialName("to_currency") val toCurrency: String,
)

/** Response of GET /v1/config — server-owned pricing facts for previews. */
@Serializable
data class ClientConfigResponse(
    @SerialName("transfer_fee_bps") val transferFeeBps: Int = 0,
    /** Cap on a single fingerprint-terminal payment (minor units); informational for the app path. */
    @SerialName("biometric_max_minor") val biometricMaxMinor: Long = 0,
    /** Default lifetime of a merchant check, seconds. */
    @SerialName("check_ttl_secs") val checkTtlSecs: Long = 300,
)

// --- Checks (DESIGN.md §20): an amount a merchant wants to collect ---

@Serializable
data class CreateCheckRequest(
    val account: String,
    @SerialName("amount_minor") val amountMinor: Long,
    val currency: String,
    val description: String? = null,
)

/**
 * A check as GET /v1/checks/{id} returns it. [status] is "open" | "paid" |
 * "cancelled" | "expired" (expiry is applied server-side, never computed here).
 */
@Serializable
data class CheckDto(
    val id: String,
    val status: String,
    @SerialName("amount_minor") val amountMinor: Long,
    val currency: String,
    val description: String? = null,
    /** The merchant's wallet (credited on payment). */
    val account: String,
    @SerialName("transaction_id") val transactionId: String? = null,
    @SerialName("payer_name") val payerName: String? = null,
    @SerialName("merchant_name") val merchantName: String? = null,
    @SerialName("created_at_ms") val createdAtMs: Long,
    @SerialName("expires_at_ms") val expiresAtMs: Long,
    @SerialName("paid_at_ms") val paidAtMs: Long? = null,
) {
    fun money(): Money = Money.ofMinor(amountMinor, Currency.of(currency))

    val isOpen: Boolean get() = status == "open"
}

/** Body of POST /v1/checks/{id}/pay; [account] = the payer's wallet (null = server picks). */
@Serializable
data class PayCheckRequest(
    val account: String? = null,
)
