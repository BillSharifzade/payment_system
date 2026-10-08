package tj.payment.wallet.ui

import tj.payment.core.ApiOutcome
import tj.payment.core.AuthDenial
import tj.payment.core.ErrorCode
import tj.payment.wallet.R

/**
 * Turns a machine error code into human copy, from string resources (English
 * default, Russian in values-ru). Server `message` strings are never shown —
 * only mapped codes.
 *
 * Exhaustive on purpose (no `else`): adding a code to [ErrorCode] does not
 * compile until it has copy here.
 */
fun ErrorCode.userMessage(): String = Copy.text(messageRes())

private fun ErrorCode.messageRes(): Int = when (this) {
    ErrorCode.INSUFFICIENT_FUNDS -> R.string.error_insufficient_funds
    ErrorCode.LIMIT_EXCEEDED -> R.string.error_limit_exceeded
    ErrorCode.KYC_REQUIRED -> R.string.error_kyc_required
    ErrorCode.ACCOUNT_BLOCKED -> R.string.error_account_blocked
    ErrorCode.RECIPIENT_UNAVAILABLE -> R.string.error_recipient_unavailable
    ErrorCode.CURRENCY_MISMATCH -> R.string.error_currency_mismatch
    ErrorCode.UNKNOWN_CURRENCY -> R.string.error_unknown_currency
    ErrorCode.UNKNOWN_ACCOUNT -> R.string.error_unknown_account
    ErrorCode.RATE_LIMITED -> R.string.error_rate_limited
    ErrorCode.RETRY_LATER -> R.string.error_retry_later
    ErrorCode.TIMEOUT -> R.string.error_timeout
    ErrorCode.UNAUTHORIZED -> R.string.error_unauthorized
    ErrorCode.FORBIDDEN -> R.string.error_forbidden
    ErrorCode.DUAL_CONTROL_REQUIRED -> R.string.error_dual_control_required
    ErrorCode.NOT_FOUND -> R.string.error_not_found
    ErrorCode.CONFLICT -> R.string.error_conflict
    ErrorCode.IDEMPOTENCY_CONFLICT -> R.string.error_idempotency_conflict
    // Only ever shown when it could not be resolved; the payment logic turns
    // this code into a status lookup, never a refusal.
    ErrorCode.DUPLICATE_TRANSACTION -> R.string.error_duplicate_transaction
    ErrorCode.VOIDED -> R.string.error_voided
    ErrorCode.REJECTED -> R.string.error_rejected
    ErrorCode.INVALID_AMOUNT, ErrorCode.AMOUNT_TOO_LARGE -> R.string.error_invalid_amount
    ErrorCode.INVALID_TRANSACTION -> R.string.error_invalid_transaction
    ErrorCode.BAD_REQUEST -> R.string.error_bad_request
    ErrorCode.NO_MATCH -> R.string.error_no_match
    ErrorCode.AMBIGUOUS_MATCH -> R.string.error_ambiguous_match
    ErrorCode.PROBE_REPLAYED -> R.string.error_probe_replayed
    ErrorCode.CHECK_LOCKED -> R.string.error_check_locked
    ErrorCode.TERMINAL_UNAUTHORIZED -> R.string.error_terminal_unauthorized
    // Both also raise the app-wide "confirm your password" prompt (DeviceEnrollment).
    ErrorCode.DEVICE_SIGNATURE_REQUIRED -> R.string.error_device_signature_required
    ErrorCode.DEVICE_SIGNATURE_INVALID -> R.string.error_device_signature_invalid
    ErrorCode.INTERNAL_ERROR -> R.string.error_internal
    ErrorCode.UNKNOWN -> R.string.error_unknown
}

/**
 * Code-mapped copy. For a server-side failure (5xx) the server's correlation id
 * is appended ("Ref: …") so support can find the exact request in the logs.
 */
fun ApiOutcome.Failed.userMessage(): String {
    val base = code.userMessage()
    val ref = requestId?.takeIf { httpStatus >= 500 && it.isNotBlank() } ?: return base
    return Copy.text(R.string.error_with_ref, base, ref)
}

/** Offline copy that tells a device-storage fault apart from a network one. */
fun ApiOutcome.Offline.userMessage(): String =
    if (localStorageFault) STORAGE_MESSAGE else OFFLINE_MESSAGE

/** Why a money move was not authorized on this device. */
fun AuthDenial.userMessage(): String = Copy.text(
    when (this) {
        AuthDenial.NOT_ENROLLED -> R.string.auth_denied_not_enrolled
        AuthDenial.UNAVAILABLE -> R.string.auth_denied_unavailable
        AuthDenial.SECURITY_UPDATE_REQUIRED -> R.string.auth_denied_security_update
        AuthDenial.LOCKED_OUT -> R.string.auth_denied_locked_out
        AuthDenial.NO_SCREEN -> R.string.auth_denied_no_screen
        AuthDenial.FAILED -> R.string.auth_denied_failed
        AuthDenial.DEVICE_NOT_REGISTERED -> R.string.auth_denied_device_not_registered
    },
)

val OFFLINE_MESSAGE: String get() = Copy.text(R.string.error_offline)

val STORAGE_MESSAGE: String get() = Copy.text(R.string.error_storage)

/** For a payment the device could not store before sending. */
val NOT_STARTED_MESSAGE: String get() = Copy.text(R.string.error_not_started)
