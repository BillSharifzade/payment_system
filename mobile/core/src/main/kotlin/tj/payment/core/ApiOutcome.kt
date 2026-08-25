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
    ) : ApiOutcome<Nothing>

    /** Never reached the server, or the response was unreadable. Safe to retry. */
    data class Offline(val cause: Throwable) : ApiOutcome<Nothing>
}

inline fun <T, R> ApiOutcome<T>.map(transform: (T) -> R): ApiOutcome<R> = when (this) {
    is ApiOutcome.Ok -> ApiOutcome.Ok(transform(value))
    is ApiOutcome.Failed -> this
    is ApiOutcome.Offline -> this
}
