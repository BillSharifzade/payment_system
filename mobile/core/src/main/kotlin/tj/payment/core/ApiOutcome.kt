package tj.payment.core

import java.io.IOException

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
        /**
         * True when [code] came from the API's own error envelope, false when it
         * was guessed from a bare HTTP status (a proxy, a gateway, a route this
         * server version doesn't have). Logic that *drops a payment record* on a
         * specific code (e.g. a coded 404 `not_found` from the void endpoint)
         * must require this: an uncoded 404 proves nothing.
         */
        val coded: Boolean = false,
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

        /** The API itself said `not_found` (404 with an error envelope). */
        val codedNotFound: Boolean get() = coded && httpStatus == 404 && code == ErrorCode.NOT_FOUND
    }

    /**
     * Never reached the server, or the connection broke mid-flight. Safe to
     * retry. Also used when the request could not be *prepared* on this device
     * because its secure storage failed (the cause is then a
     * [LocalStorageException]): nothing was sent either way.
     */
    data class Offline(val cause: Throwable) : ApiOutcome<Nothing> {
        /** The device's secure storage failed — not the network. */
        val localStorageFault: Boolean get() = cause is LocalStorageException
    }
}

inline fun <T, R> ApiOutcome<T>.map(transform: (T) -> R): ApiOutcome<R> = when (this) {
    is ApiOutcome.Ok -> ApiOutcome.Ok(transform(value))
    is ApiOutcome.Failed -> this
    is ApiOutcome.Offline -> this
}

/**
 * The device's Keystore-backed storage could not be read or written right now
 * (a Keystore fault, an unreadable keyset, a store that had to fall back to
 * memory). Typed so callers can tell it apart from "nothing stored" — which it
 * must never be mistaken for — and from a network failure. An [IOException] so
 * it can travel through OkHttp's interceptor/authenticator contracts and the
 * existing offline paths unchanged.
 */
class LocalStorageException(message: String, cause: Throwable? = null) : IOException(message, cause)
