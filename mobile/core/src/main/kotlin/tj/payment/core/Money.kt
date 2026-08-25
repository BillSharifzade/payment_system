package tj.payment.core

/**
 * A currency and how many decimal places its minor unit uses.
 * TJS: 1 somoni = 100 diram (exponent 2). Matches the backend `currencies` table.
 */
data class Currency(val code: String, val exponent: Int) {
    init {
        require(exponent in 0..6) { "implausible currency exponent: $exponent" }
    }

    companion object {
        val TJS = Currency("TJS", 2)
        val USD = Currency("USD", 2)

        fun of(code: String): Currency = when (code) {
            "TJS" -> TJS
            "USD" -> USD
            else -> Currency(code, 2)
        }
    }
}

/**
 * Money as integer minor units — never a floating-point number. The backend
 * ledger is integer-exact and this type is its client-side mirror: every amount
 * that crosses the API (`amount_minor`) is a [Long] here, all the way to the
 * formatting boundary. Arithmetic is currency-checked and overflow-checked.
 */
data class Money(val minorUnits: Long, val currency: Currency) {

    operator fun plus(other: Money): Money {
        requireSameCurrency(other)
        return Money(Math.addExact(minorUnits, other.minorUnits), currency)
    }

    operator fun minus(other: Money): Money {
        requireSameCurrency(other)
        return Money(Math.subtractExact(minorUnits, other.minorUnits), currency)
    }

    val isPositive: Boolean get() = minorUnits > 0
    val isZero: Boolean get() = minorUnits == 0L

    private fun requireSameCurrency(other: Money) =
        require(currency == other.currency) {
            "cannot mix ${currency.code} and ${other.currency.code}"
        }

    /**
     * Human string, e.g. 123456 TJS(exp 2) -> "1 234,56". Grouped by thousands
     * with a plain space, comma decimal separator (Tajik/Russian convention).
     * No currency code — callers append the localized symbol/word. Pure integer
     * math; no Double, no Locale-dependent formatting.
     */
    fun formatAmount(): String {
        val negative = minorUnits < 0
        val abs = if (negative) -minorUnits else minorUnits
        val scale = pow10(currency.exponent)
        val whole = abs / scale
        val frac = abs % scale

        val out = StringBuilder()
        if (negative) out.append('-')
        out.append(groupThousands(whole))
        if (currency.exponent > 0) {
            out.append(',')
            out.append(frac.toString().padStart(currency.exponent, '0'))
        }
        return out.toString()
    }

    /** "1 234,56 TJS" — amount plus the currency code. */
    fun format(): String = "${formatAmount()} ${currency.code}"

    companion object {
        fun ofMinor(minor: Long, currency: Currency) = Money(minor, currency)

        /**
         * Parse user-typed decimal text ("1234.56", "1 234,56", "65") into minor
         * units for [currency]. Accepts '.' or ',' as the decimal mark and any
         * whitespace as grouping. Returns null on anything malformed or with more
         * fraction digits than the currency allows — the UI treats null as "not a
         * valid amount yet" and keeps the Send button disabled. Text -> minor
         * directly; a Double never touches the value.
         */
        fun parse(input: String, currency: Currency): Money? {
            // Strip every kind of space (ASCII, NBSP, narrow NBSP, thin space).
            val cleaned = input.filterNot {
                it.isWhitespace() || it == ' ' || it == ' ' || it == ' '
            }
            if (cleaned.isEmpty()) return null

            val dot = cleaned.indexOfFirst { it == '.' || it == ',' }
            val wholePart: String
            val fracPart: String
            if (dot == -1) {
                wholePart = cleaned
                fracPart = ""
            } else {
                wholePart = cleaned.substring(0, dot)
                fracPart = cleaned.substring(dot + 1)
            }

            if (wholePart.isEmpty() && fracPart.isEmpty()) return null
            if (wholePart.isNotEmpty() && !wholePart.all { it.isDigit() }) return null
            if (!fracPart.all { it.isDigit() }) return null
            if (fracPart.length > currency.exponent) return null

            val scale = pow10(currency.exponent)
            val whole = if (wholePart.isEmpty()) 0L else wholePart.toLongOrNull() ?: return null
            val frac = fracPart.padEnd(currency.exponent, '0')
            val fracValue = if (frac.isEmpty()) 0L else frac.toLongOrNull() ?: return null

            return try {
                val minor = Math.addExact(Math.multiplyExact(whole, scale), fracValue)
                Money(minor, currency)
            } catch (_: ArithmeticException) {
                null
            }
        }

        private fun pow10(n: Int): Long {
            var r = 1L
            repeat(n) { r *= 10 }
            return r
        }

        private fun groupThousands(value: Long): String {
            val s = value.toString()
            if (s.length <= 3) return s
            val sb = StringBuilder()
            val firstGroup = s.length % 3
            var i = 0
            if (firstGroup > 0) {
                sb.append(s, 0, firstGroup)
                i = firstGroup
            }
            while (i < s.length) {
                if (sb.isNotEmpty()) sb.append(' ')
                sb.append(s, i, i + 3)
                i += 3
            }
            return sb.toString()
        }
    }
}
