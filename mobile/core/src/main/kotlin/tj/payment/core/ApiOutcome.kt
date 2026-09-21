package tj.payment.core

/**
 * The result of any API call. Exhaustive by design so callers must handle the
 * failure and offline paths a money app cannot skip.
 */
sealed interface ApiOutcome<out T> {
    data class Ok<T>(val value: T) : ApiOutcome<T>

    /** The server answered with an error envelope (or a mapped HTTP status). */
    data class Failed(
        val code: ErrorCode,
        val httpStatus: Int,
        val serverMessage: String?,
        /** Server-side correlation id (`error.request_id`), for support. */
        val requestId: String? = null,
    ) : ApiOutcome<Nothing> {
        /**
         * A 2xx whose body could not be read. The server ACCEPTED the request —
         * we just never saw its answer. Never a refusal, never a reason to drop
         * an idempotency key.
         */
        val serverAccepted: Boolean get() = httpStatus in 200..299

        /**
         * The server did not rule on the request: it accepted it unreadably,
         * was overloaded or timed out (any 5xx — incl. 503 `retry_later` and
         * 504 `timeout`), or throttled us. The only safe next step is a retry
         * with the SAME idempotency key.
         */
        val undetermined: Boolean
            get() = serverAccepted || httpStatus >= 500 ||
                code == ErrorCode.RATE_LIMITED || code == ErrorCode.RETRY_LATER ||
                code == ErrorCode.TIMEOUT
    }

    /** Never reached the server, or the connection broke mid-flight. Safe to retry. */
    data class Offline(val cause: Throwable) : ApiOutcome<Nothing>
}

inline fun <T, R> ApiOutcome<T>.map(transform: (T) -> R): ApiOutcome<R> = when (this) {
    is ApiOutcome.Ok -> ApiOutcome.Ok(transform(value))
    is ApiOutcome.Failed -> this
    is ApiOutcome.Offline -> this
}
