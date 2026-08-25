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
