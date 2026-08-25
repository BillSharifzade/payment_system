package tj.payment.core

import org.junit.Assert.assertEquals
import org.junit.Test

/** Must mirror the backend's `FeeConfig::fee_minor` exactly, or previews lie. */
class FeesTest {
    @Test
    fun `zero bps means zero fee`() {
        assertEquals(0, transferFeeMinor(1_000_00, 0))
    }

    @Test
    fun `half a percent of 2000 somoni is 10 somoni exactly`() {
        // The backend integration suite pins the same case (50 bps of 200000).
        assertEquals(10_00, transferFeeMinor(2_000_00, 50))
    }

    @Test
    fun `fee floors like the backend`() {
        // 1 bp of 99 diram = 0.0099 -> floors to 0.
        assertEquals(0, transferFeeMinor(99, 1))
        // 100 bp (1%) of 150 diram = 1.5 -> floors to 1.
        assertEquals(1, transferFeeMinor(150, 100))
    }

    @Test
    fun `non-positive amounts have no fee`() {
        assertEquals(0, transferFeeMinor(0, 50))
        assertEquals(0, transferFeeMinor(-5, 50))
    }

    @Test
    fun `overflow saturates instead of wrapping`() {
        assertEquals(Long.MAX_VALUE, transferFeeMinor(Long.MAX_VALUE, 10_000))
    }

    @Test
    fun `fx conversion floors with integer math`() {
        val rate = FxRateDto("TJS", "USD", rateNum = 917, rateDen = 10_000, updatedAtMs = 0)
        // 10.00 TJS * 0.0917 = 0.917 USD -> 91 cents, floored.
        assertEquals(91L, rate.convert(1_000))
        // Overflow -> null, never a wrapped number.
        assertEquals(null, rate.convert(Long.MAX_VALUE))
    }
}
