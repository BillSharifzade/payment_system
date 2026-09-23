package tj.payment.wallet.ui.paycheck

import java.util.UUID

/**
 * What a merchant's QR carries: `tjpay://check/<uuid>`. A bare UUID is
 * accepted too (typed or pasted from the merchant's screen).
 */
object CheckCode {
    private const val PREFIX = "tjpay://check/"

    fun encode(checkId: String): String = PREFIX + checkId

    /** The check id inside [raw], or null when it is not a check code at all. */
    fun parse(raw: String): String? {
        val candidate = raw.trim().removePrefix(PREFIX).trim()
        return try {
            UUID.fromString(candidate).toString()
        } catch (_: IllegalArgumentException) {
            null
        }
    }
}
