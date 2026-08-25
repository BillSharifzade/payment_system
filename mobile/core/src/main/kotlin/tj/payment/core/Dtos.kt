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
    @SerialName("expires_in") val expiresInSeconds: Long,
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

/** Error envelope: {"error": {"code": "...", "message": "..."}}. */
@Serializable
data class ErrorEnvelope(val error: ErrorBody) {
    @Serializable
    data class ErrorBody(
        val code: String? = null,
        val message: String? = null,
    )
}
