package tj.payment.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tj.payment.core.PendingPaymentResolver.Notice

/** Home/FX surface an unsettled payment and resolve it without opening Send. */
@OptIn(ExperimentalCoroutinesApi::class)
class PendingPaymentResolverTest {

    private val payment = PendingPayment("key-1", "from", "to", 100, "TJS", "+992900000001", userId = "alice", authorization = "sig")

    private class World(pending: PendingPayment?) {
        val store = PaymentSubmitterTest.FakeStore().apply { pending?.let { records[it.userId] = it } }
        var execute: (PendingPayment) -> ApiOutcome<Settlement> = { PaymentSubmitterTest.posted("key-1", alreadyPosted = true) }
        var lookup: (String) -> ApiOutcome<TransactionStatusDto> = { PaymentSubmitterTest.notFound() }
        var void: (String) -> ApiOutcome<TransactionStatusDto> = { ApiOutcome.Ok(PaymentSubmitterTest.voided(it)) }
        var resolvedCalls = 0
        val submitter = PaymentSubmitter(
            store = store,
            currentUser = { "alice" },
            newKey = { "unused" },
            authorizer = { Authorization.Granted("sig") },
            execute = { execute(it) },
            lookup = { lookup(it) },
            void = { void(it) },
            io = Dispatchers.Unconfined,
        )

        fun resolver(scope: TestScope) = PendingPaymentResolver(submitter, scope) { resolvedCalls++ }
    }

    @Test
    fun `load shows the pending payment and a not-yet-posted check keeps it`() = runTest {
        val w = World(payment)
        val r = w.resolver(this)
        r.load()
        advanceUntilIdle()
        assertEquals(payment, r.state.value.pending)
        assertNull(r.state.value.busy)
        assertNull(r.state.value.notice)
    }

    @Test
    fun `load resolves a payment that already posted, without the user doing anything`() = runTest {
        val w = World(payment).apply { lookup = { ApiOutcome.Ok(PaymentSubmitterTest.postedStatus(it)) } }
        val r = w.resolver(this)
        r.load()
        advanceUntilIdle()
        assertNull(r.state.value.pending)
        assertEquals(Notice.Sent(payment), r.state.value.notice)
        assertEquals(1, w.resolvedCalls)
        r.dismissNotice()
        assertNull(r.state.value.notice)
    }

    @Test
    fun `load without check never calls the server`() = runTest {
        var lookups = 0
        val w = World(payment).apply { lookup = { lookups++; PaymentSubmitterTest.notFound() } }
        val r = w.resolver(this)
        r.load(check = false)
        advanceUntilIdle()
        assertEquals(payment, r.state.value.pending)
        assertEquals(0, lookups)
    }

    @Test
    fun `finish retries with the same key and reports the outcome`() = runTest {
        val w = World(payment)
        val r = w.resolver(this)
        r.load(check = false)
        advanceUntilIdle()

        w.execute = { ApiOutcome.Offline(RuntimeException("down")) }
        r.finish()
        advanceUntilIdle()
        assertEquals(Notice.NoAnswer(offline = true), r.state.value.notice)
        assertEquals(payment, r.state.value.pending)

        w.execute = { p -> PaymentSubmitterTest.posted(p.idempotencyKey, alreadyPosted = true) }
        r.finish()
        advanceUntilIdle()
        assertEquals(Notice.Sent(payment), r.state.value.notice)
        assertNull(r.state.value.pending)
        assertEquals(1, w.resolvedCalls)
    }

    @Test
    fun `finish answered voided reports it as cancelled`() = runTest {
        val w = World(payment).apply { execute = { ApiOutcome.Failed(ErrorCode.VOIDED, 409, null, coded = true) } }
        val r = w.resolver(this)
        r.load(check = false)
        advanceUntilIdle()
        r.finish()
        advanceUntilIdle()
        assertEquals(Notice.Cancelled(payment), r.state.value.notice)
        assertNull(r.state.value.pending)
    }

    @Test
    fun `discard asks first, voids, and keeps the record when the void is inconclusive`() = runTest {
        val w = World(payment).apply { void = { ApiOutcome.Offline(RuntimeException("down")) } }
        val r = w.resolver(this)
        r.load(check = false)
        advanceUntilIdle()

        r.requestDiscard()
        assertTrue(r.state.value.confirmDiscard)
        r.cancelDiscard()
        assertFalse(r.state.value.confirmDiscard)

        r.requestDiscard()
        r.confirmDiscard()
        advanceUntilIdle()
        assertEquals(Notice.DiscardFailed(offline = true), r.state.value.notice)
        assertEquals(payment, r.state.value.pending)

        w.void = { ApiOutcome.Ok(PaymentSubmitterTest.voided(it)) }
        r.requestDiscard()
        r.confirmDiscard()
        advanceUntilIdle()
        assertEquals(Notice.Cancelled(payment), r.state.value.notice)
        assertNull(r.state.value.pending)
    }

    @Test
    fun `a discard that finds the payment posted says so`() = runTest {
        val w = World(payment).apply { void = { ApiOutcome.Ok(PaymentSubmitterTest.postedStatus(it)) } }
        val r = w.resolver(this)
        r.load(check = false)
        advanceUntilIdle()
        r.requestDiscard()
        r.confirmDiscard()
        advanceUntilIdle()
        assertEquals(Notice.Sent(payment), r.state.value.notice)
    }

    @Test
    fun `nothing pending shows nothing`() = runTest {
        val w = World(null)
        val r = w.resolver(this)
        r.load()
        advanceUntilIdle()
        assertNull(r.state.value.pending)
        r.finish()
        r.requestDiscard()
        assertFalse(r.state.value.confirmDiscard)
    }
}
