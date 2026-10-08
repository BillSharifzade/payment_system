package tj.payment.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
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

    /** The hardening contract's §9: the complete list every client must handle. */
    @Test
    fun every_contract_error_code_is_known() {
        val contract = listOf(
            "bad_request", "not_found", "unauthorized", "forbidden", "conflict", "idempotency_conflict",
            "kyc_required", "account_blocked", "limit_exceeded", "insufficient_funds", "no_match",
            "ambiguous_match", "rate_limited", "retry_later", "timeout", "internal_error",
            "invalid_transaction", "currency_mismatch", "unknown_account", "duplicate_transaction",
            "invalid_amount", "unknown_currency", "amount_too_large", "rejected", "voided",
            "dual_control_required", "recipient_unavailable", "terminal_unauthorized",
            "probe_replayed", "check_locked",
        )
        for (wire in contract) {
            val code = ErrorCode.fromWire(wire)
            assertNotEquals("$wire must be mapped", ErrorCode.UNKNOWN, code)
            assertEquals(wire, code.wire)
        }
        assertEquals("no stale codes beyond the contract", contract.toSet() + "unknown", ErrorCode.entries.map { it.wire }.toSet())
    }

    @Test
    fun a_bare_status_never_invents_a_business_refusal() {
        // A 422 without an envelope (proxy, framework rejection) is NOT limit_exceeded.
        assertEquals(ErrorCode.UNKNOWN, ErrorCode.fromResponse(422, null))
        assertEquals(ErrorCode.LIMIT_EXCEEDED, ErrorCode.fromResponse(422, "limit_exceeded"))
        assertEquals(ErrorCode.UNAUTHORIZED, ErrorCode.fromResponse(401, null))
        assertEquals(ErrorCode.NOT_FOUND, ErrorCode.fromResponse(404, null))
        assertEquals(ErrorCode.RETRY_LATER, ErrorCode.fromResponse(503, null))
        assertEquals(ErrorCode.TIMEOUT, ErrorCode.fromResponse(504, null))
        assertEquals(ErrorCode.UNKNOWN, ErrorCode.fromResponse(418, null))
        // The envelope wins over the status.
        assertEquals(ErrorCode.RECIPIENT_UNAVAILABLE, ErrorCode.fromResponse(403, "recipient_unavailable"))
    }

    @Test
    fun formatting_is_total_including_long_min_value() {
        assertEquals("-92 233 720 368 547 758,08", Money.ofMinor(Long.MIN_VALUE, tjs).formatAmount())
        assertEquals("92 233 720 368 547 758,07", Money.ofMinor(Long.MAX_VALUE, tjs).formatAmount())
        assertEquals("-0,01", Money.ofMinor(-1, tjs).formatAmount())
        assertEquals("0,00", Money.ofMinor(0, tjs).formatAmount())
    }

    @Test
    fun unknown_currencies_fail_closed() {
        val xyz = Currency.of("XYZ")
        assertFalse(xyz.isKnown)
        assertNull(Currency.knownOrNull("XYZ"))
        assertEquals(Currency.TJS, Currency.knownOrNull("TJS"))
        // Never parsed: no amount can be entered, so no money moves with a guessed scale.
        assertNull(Money.parse("10", xyz))
        assertNull(Money.parse("10.00", xyz))
        // Displayed exactly, as minor units, and labelled so.
        assertEquals("12 345", Money.ofMinor(12_345, xyz).formatAmount())
        assertEquals("12 345 XYZ (minor units)", Money.ofMinor(12_345, xyz).format())
        val wallet = WalletDto("w", "XYZ", 12_345, display = "123.45 XYZ")
        assertEquals("123.45 XYZ", wallet.displayAmount())
        assertEquals("1 234,56", WalletDto("w", "TJS", 123_456, display = "ignored").displayAmount())
    }
}
