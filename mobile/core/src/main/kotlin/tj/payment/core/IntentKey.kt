package tj.payment.core

/**
 * One idempotency key per **intent**, for requests whose key lives in memory
 * (creating a merchant check). The rules, mirroring the server's:
 *
 *  - the same intent (equal [I]) after an *unknown* outcome reuses its key, so
 *    a retry can never create a second resource;
 *  - a *different* intent (amount, wallet, description changed) gets a new key
 *    — reusing the old one would make the server answer 409
 *    `idempotency_conflict` forever;
 *  - a *definitive* outcome (success, or a refusal the server ruled on) ends
 *    the intent: the next attempt is a new request with a new key.
 */
class IntentKey<I : Any>(private val newKey: () -> String) {
    private var current: Pair<I, String>? = null

    /** The key for [intent]: the held one if it is still the same unsettled intent, else a new one. */
    @Synchronized
    fun keyFor(intent: I): String {
        current?.let { (held, key) -> if (held == intent) return key }
        return newKey().also { current = intent to it }
    }

    /** Apply a request's outcome: keep the key only when the server may not have ruled. */
    @Synchronized
    fun onOutcome(outcome: ApiOutcome<*>) {
        val keep = when (outcome) {
            is ApiOutcome.Ok -> false
            is ApiOutcome.Failed -> outcome.undetermined
            is ApiOutcome.Offline -> true
        }
        if (!keep) current = null
    }

    /** Forget any held key (the screen was reset for a new request). */
    @Synchronized
    fun reset() {
        current = null
    }

    /** The key held for an unsettled intent, if any (for tests and diagnostics). */
    @get:Synchronized
    val heldKey: String? get() = current?.second
}
