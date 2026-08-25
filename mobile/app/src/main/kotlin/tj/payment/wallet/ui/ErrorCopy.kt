package tj.payment.wallet.ui

import tj.payment.core.ApiOutcome
import tj.payment.core.ErrorCode

/**
 * Turns a machine error code into human copy. English for now; these become the
 * keys for Tajik/Russian string resources in the localization pass. Server
 * `message` strings are never shown — only mapped codes.
 */
fun ErrorCode.userMessage(): String = when (this) {
    ErrorCode.INSUFFICIENT_FUNDS -> "Not enough balance for this transfer."
    ErrorCode.LIMIT_EXCEEDED -> "This exceeds your current limit."
    ErrorCode.KYC_REQUIRED -> "Verify your identity to continue."
    ErrorCode.ACCOUNT_BLOCKED -> "This account is blocked. Contact support."
    ErrorCode.CURRENCY_MISMATCH -> "The wallets use different currencies."
    ErrorCode.RATE_LIMITED -> "Too many attempts. Please wait a moment."
    ErrorCode.UNAUTHORIZED -> "Your session expired. Please sign in again."
    ErrorCode.FORBIDDEN -> "You don't have access to this."
    ErrorCode.NOT_FOUND -> "Not found."
    ErrorCode.CONFLICT, ErrorCode.IDEMPOTENCY_CONFLICT -> "That request conflicts with another. Try again."
    ErrorCode.INVALID_AMOUNT, ErrorCode.AMOUNT_TOO_LARGE -> "That amount isn't valid."
    ErrorCode.BAD_REQUEST -> "Something about that request was invalid."
    else -> "Something went wrong. Please try again."
}

fun ApiOutcome.Failed.userMessage(): String = code.userMessage()

const val OFFLINE_MESSAGE = "No connection. Check your network and try again."
