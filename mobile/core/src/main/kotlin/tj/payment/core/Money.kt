package tj.payment.core

/**
 * A currency and how many decimal places its minor unit uses.
 * TJS: 1 somoni = 100 diram (exponent 2). Mirrors the backend `currencies`
 * table (migrations 0002, 0012) — the client never guesses an exponent.
 *
 * [isKnown] is false for a code this build has no exponent for (the backend
 * added a currency after this app shipped). Such a currency fails closed: its
 * amounts can be displayed only as raw minor units and can never be parsed
 * from user input, so no money can be moved with a guessed scale.
 */
data class Currency(val code: String, val exponent: Int, val isKnown: Boolean = true) {
    init {
        require(exponent in 0..6) { "implausible currency exponent: $exponent" }
    }

    companion object {
        val TJS = Currency("TJS", 2)
        val USD = Currency("USD", 2)

        private val KNOWN = listOf(TJS, USD).associateBy { it.code }

        /** The known currency for [code], or null. */
        fun knownOrNull(code: String): Currency? = KNOWN[code]

        /**
         * The known currency for [code]; otherwise an *unknown* marker
         * (exponent 0, [isKnown] false) — never a guessed exponent.
         */
        fun of(code: String): Currency = KNOWN[code] ?: Currency(code, exponent = 0, isKnown = false)
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
     * math; no Double, no Locale-dependent formatting. Total over every Long,
     * including [Long.MIN_VALUE] (whose magnitude does not fit in a Long).
     *
     * An unknown currency (see [Currency.isKnown]) renders its raw minor units
     * with no decimal separator: exact, never a guessed scale.
     */
    fun formatAmount(): String {
        val negative = minorUnits < 0
        // Magnitude as unsigned: -Long.MIN_VALUE wraps to itself, whose
        // unsigned reading is exactly 2^63.
        val abs: ULong = if (negative) (-minorUnits).toULong() else minorUnits.toULong()
        val scale = pow10(currency.exponent).toULong()
        val whole = abs / scale
        val frac = abs % scale

        val out = StringBuilder()
        if (negative) out.append('-')
        out.append(groupThousands(whole.toString()))
        if (currency.exponent > 0) {
            out.append(',')
            out.append(frac.toString().padStart(currency.exponent, '0'))
        }
        return out.toString()
    }

    /**
     * "1 234,56 TJS" — amount plus the currency code. An unknown currency says
     * its number is in minor units, so nobody reads 12345 diram as 12345 somoni.
     */
    fun format(): String =
        if (currency.isKnown) "${formatAmount()} ${currency.code}" else "${formatAmount()} ${currency.code} (minor units)"

    companion object {
        fun ofMinor(minor: Long, currency: Currency) = Money(minor, currency)

        /**
         * Parse user-typed decimal text ("1234.56", "1 234,56", "65") into minor
         * units for [currency]. Accepts '.' or ',' as the decimal mark and any
         * whitespace as grouping. Returns null on anything malformed or with more
         * fraction digits than the currency allows — the UI treats null as "not a
         * valid amount yet" and keeps the Send button disabled. Text -> minor
         * directly; a Double never touches the value. An unknown currency never
         * parses (fail closed: its scale is not known to this build).
         */
        fun parse(input: String, currency: Currency): Money? {
            if (!currency.isKnown) return null
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

        private fun groupThousands(s: String): String {
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
