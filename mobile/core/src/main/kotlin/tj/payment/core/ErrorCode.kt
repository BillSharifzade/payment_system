package tj.payment.core

/**
 * The backend's stable machine error codes (from `crates/api/src/error.rs`).
 * The client maps *these*, never the English `message` strings, which are for
 * logs and unstable. UI copy for each code lives in the presentation layer and
 * is localized (Tajik/Russian); this enum is the contract.
 */
enum class ErrorCode(val wire: String) {
    ACCOUNT_BLOCKED("account_blocked"),
    AMOUNT_TOO_LARGE("amount_too_large"),
    BAD_REQUEST("bad_request"),
    CONFLICT("conflict"),
    CURRENCY_MISMATCH("currency_mismatch"),
    DUPLICATE_TRANSACTION("duplicate_transaction"),
    FORBIDDEN("forbidden"),
    IDEMPOTENCY_CONFLICT("idempotency_conflict"),
    INSUFFICIENT_FUNDS("insufficient_funds"),
    INTERNAL_ERROR("internal_error"),
    INVALID_AMOUNT("invalid_amount"),
    INVALID_TRANSACTION("invalid_transaction"),
    KYC_REQUIRED("kyc_required"),
    LIMIT_EXCEEDED("limit_exceeded"),
    NOT_FOUND("not_found"),
    RATE_LIMITED("rate_limited"),
    UNAUTHORIZED("unauthorized"),
    UNKNOWN_ACCOUNT("unknown_account"),
    UNKNOWN_CURRENCY("unknown_currency"),

    /** Any code we don't recognize, or a non-JSON/transport failure. */
    UNKNOWN("unknown");

    companion object {
        fun fromWire(wire: String?): ErrorCode =
            entries.firstOrNull { it.wire == wire } ?: UNKNOWN
    }
}
