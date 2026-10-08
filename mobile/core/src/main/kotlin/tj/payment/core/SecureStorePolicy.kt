package tj.payment.core

/** Whether a secure-storage failure can heal by itself. */
enum class StorageFault {
    /** May pass (a busy/restarting Keystore, a transient binder error): retry, never destroy data. */
    TRANSIENT,

    /** Will not pass (an undecryptable keyset, a corrupt file, an invalidated key): recreate. */
    PERMANENT,
}

object StorageFaults {
    /**
     * Exceptions that mean the stored keyset/file can never be read again.
     * Matched by simple class name anywhere in the cause chain, so the shaded
     * Tink/protobuf copies and the Android-only classes match without
     * depending on them.
     */
    private val PERMANENT_NAMES = setOf(
        "AEADBadTagException",
        "BadPaddingException",
        "InvalidProtocolBufferException",
        "KeyPermanentlyInvalidatedException",
        "UnrecoverableKeyException",
    )

    /**
     * Default classification: [StorageFault.PERMANENT] only on positive
     * evidence of corruption; anything unrecognized is [StorageFault.TRANSIENT]
     * — a wrong "transient" costs a degraded launch, a wrong "permanent" costs
     * the user's data (a pending payment's key).
     */
    fun classify(error: Throwable): StorageFault {
        var cause: Throwable? = error
        var depth = 0
        while (cause != null && depth < 16) {
            if (cause.javaClass.simpleName in PERMANENT_NAMES) return StorageFault.PERMANENT
            cause = cause.cause
            depth++
        }
        return StorageFault.TRANSIENT
    }
}

/**
 * How a Keystore-backed store is opened without ever destroying data on a
 * transient fault (the old code deleted the file AND the shared master key on
 * any exception — including a momentary Keystore hiccup — losing pending
 * payment keys and breaking the other store).
 *
 * 1. Try to open, retrying transient failures with backoff.
 * 2. Still failing but only transiently → [Result.Unavailable]: nothing is
 *    deleted; the caller runs memory-only for now (and refuses to start
 *    payments) and tries again later.
 * 3. Positive evidence of corruption — or the same failure across
 *    [escalateAfterFailedLaunches] consecutive launches → wipe THIS store's
 *    file and recreate; if that fails too, reset THIS store's own master key
 *    (each store has its own alias, so no other store is affected) and try once
 *    more. Either way the result is marked `recovered`: whatever it held is gone.
 *
 * Pure logic; the platform calls are injected.
 */
class SecureStoreOpener(
    private val attempts: Int = 3,
    private val backoffMs: (attempt: Int) -> Long = { attempt -> 100L shl attempt },
    private val sleep: (Long) -> Unit = { Thread.sleep(it) },
    private val classify: (Throwable) -> StorageFault = StorageFaults::classify,
    private val escalateAfterFailedLaunches: Int = 3,
) {
    init {
        require(attempts >= 1)
    }

    sealed interface Result<out T> {
        data class Opened<T>(val value: T, val recovered: Boolean) : Result<T>

        data class Unavailable(val cause: Throwable) : Result<Nothing>
    }

    /**
     * @param create Opens the store (throws on failure).
     * @param wipe Deletes this store's file.
     * @param resetKey Deletes this store's own master key.
     * @param failedLaunches Consecutive earlier launches whose open ended [Result.Unavailable].
     */
    fun <T> open(
        create: () -> T,
        wipe: () -> Unit,
        resetKey: () -> Unit,
        failedLaunches: Int = 0,
    ): Result<T> {
        var lastError: Throwable? = null

        fun attempt(): T? {
            for (i in 0 until attempts) {
                try {
                    return create()
                } catch (e: Exception) {
                    lastError = e
                    if (classify(e) == StorageFault.PERMANENT) return null
                    if (i < attempts - 1) sleep(backoffMs(i))
                }
            }
            return null
        }

        fun lastIsPermanent() = lastError?.let { classify(it) == StorageFault.PERMANENT } == true

        attempt()?.let { return Result.Opened(it, recovered = false) }
        val escalated = failedLaunches + 1 >= escalateAfterFailedLaunches
        if (!lastIsPermanent() && !escalated) return Result.Unavailable(lastError!!)

        wipe()
        attempt()?.let { return Result.Opened(it, recovered = true) }
        if (!lastIsPermanent() && !escalated) return Result.Unavailable(lastError!!)

        resetKey()
        wipe()
        attempt()?.let { return Result.Opened(it, recovered = true) }
        return Result.Unavailable(lastError!!)
    }
}
