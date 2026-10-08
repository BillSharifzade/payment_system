package tj.payment.core

/**
 * Client-side mirror of the backend's `FeeConfig::fee_minor`: fee (in minor
 * units) for a transfer of [amountMinor] at [bps] basis points, floored, and
 * saturating instead of wrapping on absurd configuration. Display-only — the
 * server remains the authority on what is actually charged — but the preview
 * must still match the posted result exactly, or the confirm screen lies.
 */
fun transferFeeMinor(amountMinor: Long, bps: Int): Long {
    if (amountMinor <= 0 || bps <= 0) return 0
    return try {
        Math.multiplyExact(amountMinor, bps.toLong()) / 10_000
    } catch (_: ArithmeticException) {
        Long.MAX_VALUE
    }
}

/**
 * The fee line a confirm screen may show, or null when there is none to show.
 * The backend charges the transfer fee on **TJS only** (`payments.rs`,
 * `biometric.rs`: `if currency.code() == "TJS"`); any other currency moves
 * fee-free, so previewing a fee there would promise a deduction that never
 * happens. Null also while the fee rate is unknown.
 */
fun transferFeePreviewMinor(amountMinor: Long, currency: String, bps: Int?): Long? {
    if (bps == null || currency != FEE_CURRENCY) return null
    return transferFeeMinor(amountMinor, bps)
}

/** The only currency the backend charges a transfer fee in. */
const val FEE_CURRENCY = "TJS"
