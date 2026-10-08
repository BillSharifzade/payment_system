package tj.payment.core

/**
 * The backend's stable machine error codes (`crates/api/src/error.rs`, plus the
 * hardening-pass additions — the complete list clients must handle). The client
 * maps *these*, never the English `message` strings, which are for logs and
 * unstable. UI copy for each code lives in the presentation layer and is
 * localized; this enum is the contract.
 */
enum class ErrorCode(val wire: String) {
    ACCOUNT_BLOCKED("account_blocked"),
    /** Fingerprint check: one probe matched more than one person (409). */
    AMBIGUOUS_MATCH("ambiguous_match"),
    AMOUNT_TOO_LARGE("amount_too_large"),
    BAD_REQUEST("bad_request"),
    /** Fingerprint check: too many failed attempts; the check was cancelled (409). */
    CHECK_LOCKED("check_locked"),
    CONFLICT("conflict"),
    CURRENCY_MISMATCH("currency_mismatch"),
    /** Admin: a second person must approve (403). */
    DUAL_CONTROL_REQUIRED("dual_control_required"),
    DUPLICATE_TRANSACTION("duplicate_transaction"),
    FORBIDDEN("forbidden"),
    IDEMPOTENCY_CONFLICT("idempotency_conflict"),
    INSUFFICIENT_FUNDS("insufficient_funds"),
    INTERNAL_ERROR("internal_error"),
    INVALID_AMOUNT("invalid_amount"),
    INVALID_TRANSACTION("invalid_transaction"),
    KYC_REQUIRED("kyc_required"),
    LIMIT_EXCEEDED("limit_exceeded"),
    /** Fingerprint check: no enrolled fingerprint matched (404). */
    NO_MATCH("no_match"),
    NOT_FOUND("not_found"),
    /** Fingerprint check: byte-identical replay of an earlier probe (409). */
    PROBE_REPLAYED("probe_replayed"),
    RATE_LIMITED("rate_limited"),
    /** The recipient's account is not active (403). */
    RECIPIENT_UNAVAILABLE("recipient_unavailable"),
    /** The ledger refused the transaction (422) — a definitive refusal. */
    REJECTED("rejected"),
    /** 503 from a lock/statement timeout on a money endpoint: same key, try again. */
    RETRY_LATER("retry_later"),
    /** Fingerprint terminal key missing, revoked or not the check's merchant (401). */
    TERMINAL_UNAUTHORIZED("terminal_unauthorized"),
    /** 504: the money endpoint ran out of time server-side. Same key, try again. */
    TIMEOUT("timeout"),
    UNAUTHORIZED("unauthorized"),
    UNKNOWN_ACCOUNT("unknown_account"),
    UNKNOWN_CURRENCY("unknown_currency"),
    /**
     * 409: this idempotency key was voided (POST /v1/transactions/{id}/void),
     * so it can never post. Definitive — nothing was or will be sent with it.
     */
    VOIDED("voided"),

    /** Any code we don't recognize, or a non-JSON/transport failure. */
    UNKNOWN("unknown");

    companion object {
        fun fromWire(wire: String?): ErrorCode =
            entries.firstOrNull { it.wire == wire } ?: UNKNOWN

        /**
         * The code for an error response: the envelope's `error.code` when there
         * is one, otherwise a conservative guess from the HTTP status alone.
         * The guess never invents a *business* refusal: a bare 422 (a proxy, a
         * framework rejection, an unparsable body) is UNKNOWN, not
         * `limit_exceeded` — telling a user they hit a limit they didn't hit is
         * worse than a generic error.
         */
        fun fromResponse(httpStatus: Int, envelopeCode: String?): ErrorCode {
            if (envelopeCode != null) return fromWire(envelopeCode)
            return when (httpStatus) {
                400 -> BAD_REQUEST
                401 -> UNAUTHORIZED
                403 -> FORBIDDEN
                404 -> NOT_FOUND
                409 -> CONFLICT
                429 -> RATE_LIMITED
                500 -> INTERNAL_ERROR
                503 -> RETRY_LATER
                504 -> TIMEOUT
                else -> UNKNOWN
            }
        }
    }
}
