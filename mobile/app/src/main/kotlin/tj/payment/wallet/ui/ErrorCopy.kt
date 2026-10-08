package tj.payment.wallet.ui

import tj.payment.core.ApiOutcome
import tj.payment.core.AuthDenial
import tj.payment.core.ErrorCode

/**
 * Turns a machine error code into human copy. English for now; these become the
 * keys for Tajik/Russian string resources in the localization pass. Server
 * `message` strings are never shown — only mapped codes.
 *
 * Exhaustive on purpose (no `else`): adding a code to [ErrorCode] does not
 * compile until it has copy here.
 */
fun ErrorCode.userMessage(): String = when (this) {
    ErrorCode.INSUFFICIENT_FUNDS -> "Not enough balance for this transfer."
    ErrorCode.LIMIT_EXCEEDED -> "This exceeds your current limit."
    ErrorCode.KYC_REQUIRED -> "Verify your identity to continue."
    ErrorCode.ACCOUNT_BLOCKED -> "This account is blocked. Contact support."
    ErrorCode.RECIPIENT_UNAVAILABLE -> "The recipient's account can't receive money right now."
    ErrorCode.CURRENCY_MISMATCH -> "The wallets use different currencies."
    ErrorCode.UNKNOWN_CURRENCY -> "This currency isn't supported."
    ErrorCode.UNKNOWN_ACCOUNT -> "That wallet doesn't exist."
    ErrorCode.RATE_LIMITED -> "Too many attempts. Please wait a moment."
    ErrorCode.RETRY_LATER -> "The service is busy right now. Please try again in a moment."
    ErrorCode.TIMEOUT -> "The service took too long to answer. Please try again in a moment."
    ErrorCode.UNAUTHORIZED -> "Your session expired. Please sign in again."
    ErrorCode.FORBIDDEN -> "You don't have access to this."
    ErrorCode.DUAL_CONTROL_REQUIRED -> "A second person has to approve this."
    ErrorCode.NOT_FOUND -> "Not found."
    ErrorCode.CONFLICT -> "That request conflicts with another. Try again."
    ErrorCode.IDEMPOTENCY_CONFLICT -> "That request conflicts with an earlier one. Start again."
    // Only ever shown when it could not be resolved; the payment logic turns
    // this code into a status lookup, never a refusal.
    ErrorCode.DUPLICATE_TRANSACTION -> "This payment was already processed. Check your history."
    ErrorCode.VOIDED -> "This payment was cancelled, so nothing was sent."
    ErrorCode.REJECTED -> "The payment was declined."
    ErrorCode.INVALID_AMOUNT, ErrorCode.AMOUNT_TOO_LARGE -> "That amount isn't valid."
    ErrorCode.INVALID_TRANSACTION -> "This payment can't be made as entered."
    ErrorCode.BAD_REQUEST -> "Something about that request was invalid."
    ErrorCode.NO_MATCH -> "The fingerprint didn't match."
    ErrorCode.AMBIGUOUS_MATCH -> "The fingerprint matched more than one person. Use another finger."
    ErrorCode.PROBE_REPLAYED -> "That fingerprint scan was already used. Scan again."
    ErrorCode.CHECK_LOCKED -> "Too many failed attempts. This payment request was cancelled."
    ErrorCode.TERMINAL_UNAUTHORIZED -> "This payment terminal isn't authorised."
    ErrorCode.INTERNAL_ERROR -> "Something went wrong on our side. Please try again."
    ErrorCode.UNKNOWN -> "Something went wrong. Please try again."
}

/**
 * Code-mapped copy. For a server-side failure (5xx) the server's correlation id
 * is appended as "Ref: …" so support can find the exact request in the logs.
 */
fun ApiOutcome.Failed.userMessage(): String {
    val base = code.userMessage()
    val ref = requestId?.takeIf { httpStatus >= 500 && it.isNotBlank() } ?: return base
    return "$base Ref: $ref"
}

/** Offline copy that tells a device-storage fault apart from a network one. */
fun ApiOutcome.Offline.userMessage(): String =
    if (localStorageFault) STORAGE_MESSAGE else OFFLINE_MESSAGE

/** Why a money move was not authorized on this device. */
fun AuthDenial.userMessage(): String = when (this) {
    AuthDenial.NOT_ENROLLED ->
        "Set up a fingerprint or a screen lock (PIN, pattern or password) in your phone's settings to move money."
    AuthDenial.UNAVAILABLE -> "This phone can't confirm it's you right now, so the payment wasn't sent."
    AuthDenial.SECURITY_UPDATE_REQUIRED -> "Your phone needs a security update before it can confirm payments."
    AuthDenial.LOCKED_OUT -> "Too many attempts. Unlock your phone with its PIN, pattern or password, then try again."
    AuthDenial.NO_SCREEN -> "Couldn't show the confirmation. Open the app and try again."
    AuthDenial.FAILED -> "Couldn't confirm it's you, so the payment wasn't sent. Try again."
}

const val OFFLINE_MESSAGE = "No connection. Check your network and try again."

const val STORAGE_MESSAGE =
    "Your phone's secure storage isn't available right now, so nothing was sent. Restart the app and try again."

/** For a payment the device could not store before sending. */
const val NOT_STARTED_MESSAGE =
    "Couldn't save this payment securely on your device, so nothing was sent. Please try again."
