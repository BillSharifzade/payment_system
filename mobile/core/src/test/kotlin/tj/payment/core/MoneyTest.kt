package tj.payment.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class MoneyTest {

    private val tjs = Currency.TJS

    @Test
    fun formats_minor_units_with_grouping_and_comma() {
        assertEquals("1 234,56", Money.ofMinor(123456, tjs).formatAmount())
        assertEquals("0,05", Money.ofMinor(5, tjs).formatAmount())
        assertEquals("65,00", Money.ofMinor(6500, tjs).formatAmount())
        assertEquals("1 000 000,00", Money.ofMinor(100_000_000, tjs).formatAmount())
        assertEquals("-12,30", Money.ofMinor(-1230, tjs).formatAmount())
        assertEquals("1 234,56 TJS", Money.ofMinor(123456, tjs).format())
    }

    @Test
    fun parses_decimal_text_into_exact_minor_units() {
        assertEquals(Money.ofMinor(123456, tjs), Money.parse("1234.56", tjs))
        assertEquals(Money.ofMinor(123456, tjs), Money.parse("1234,56", tjs))
        assertEquals(Money.ofMinor(123456, tjs), Money.parse("1 234,56", tjs))
        assertEquals(Money.ofMinor(6500, tjs), Money.parse("65", tjs))
        assertEquals(Money.ofMinor(50, tjs), Money.parse("0.5", tjs))
        assertEquals(Money.ofMinor(5, tjs), Money.parse("0.05", tjs))
    }

    @Test
    fun rejects_malformed_or_over_precise_input() {
        assertNull(Money.parse("", tjs))
        assertNull(Money.parse("abc", tjs))
        assertNull(Money.parse("1.234", tjs))   // 3 fraction digits, exp is 2
        assertNull(Money.parse("1..2", tjs))
        assertNull(Money.parse("-5", tjs))       // sign not accepted from user input
        assertNull(Money.parse("1,2,3", tjs))
    }

    @Test
    fun round_trips_parse_then_format() {
        val m = Money.parse("2 000,00", tjs)!!
        assertEquals("2 000,00", m.formatAmount())
        assertEquals(200_000L, m.minorUnits)
    }

    @Test
    fun arithmetic_is_currency_checked_and_exact() {
        val a = Money.ofMinor(10_000, tjs)
        val b = Money.ofMinor(2_550, tjs)
        assertEquals(Money.ofMinor(12_550, tjs), a + b)
        assertEquals(Money.ofMinor(7_450, tjs), a - b)
        assertTrue((a - b).isPositive)
        assertFalse(Money.ofMinor(0, tjs).isPositive)
        assertTrue(Money.ofMinor(0, tjs).isZero)
    }

    @Test(expected = IllegalArgumentException::class)
    fun refuses_to_mix_currencies() {
        Money.ofMinor(100, Currency.TJS) + Money.ofMinor(100, Currency.USD)
    }

    @Test
    fun error_codes_map_from_wire() {
        assertEquals(ErrorCode.INSUFFICIENT_FUNDS, ErrorCode.fromWire("insufficient_funds"))
        assertEquals(ErrorCode.KYC_REQUIRED, ErrorCode.fromWire("kyc_required"))
        assertEquals(ErrorCode.UNKNOWN, ErrorCode.fromWire("something_new"))
        assertEquals(ErrorCode.UNKNOWN, ErrorCode.fromWire(null))
    }
}
