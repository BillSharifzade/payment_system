package tj.payment.core

import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/**
 * This install's payment-signing key as registered for one user
 * (POST /v1/devices). The server only accepts a money move signed by an active
 * registered device of the caller; [deviceId] goes out as `X-Device-Id` next to
 * the signature.
 */
@Serializable
data class DeviceRegistration(
    @SerialName("user_id") val userId: String,
    @SerialName("device_id") val deviceId: String,
    /** Base64 SPKI DER of the key, exactly as this app exported and sent it. */
    @SerialName("public_key") val publicKey: String,
)

/** Durable per-user registration (the app keeps it in the encrypted session store). */
interface DeviceRegistrationStore {
    /** @throws LocalStorageException when the store cannot be read right now. */
    fun registration(userId: String): DeviceRegistration?

    /** @throws LocalStorageException on a storage fault. */
    fun saveRegistration(registration: DeviceRegistration)

    /** @throws LocalStorageException on a storage fault. */
    fun clearRegistration(userId: String)
}

/** What a registration attempt came to. */
sealed interface DeviceRegistrationResult {
    /** This phone's key is the user's active device; payments can be signed. */
    data object Registered : DeviceRegistrationResult

    /** The server refused the password (403 `forbidden`; also an inactive account). */
    data object WrongPassword : DeviceRegistrationResult

    /**
     * The account already has the maximum number of active devices (409). The
     * password was correct; [DeviceEnrollment.register] with `replaceOldest`
     * revokes the least recently used one and registers this phone.
     */
    data object LimitReached : DeviceRegistrationResult

    /** No screen lock / no Keystore key on this phone: nothing to register. */
    data object NoDeviceLock : DeviceRegistrationResult

    /** Registered on the server, but this phone could not store the result. */
    data object StorageUnavailable : DeviceRegistrationResult

    /** Anything else: a coded refusal (rate limit, blocked…) or no answer ([offline]). */
    data class Failed(val code: ErrorCode, val offline: Boolean = false) : DeviceRegistrationResult
}

/**
 * Device binding, client half (DESIGN.md §11): the phone's Keystore signing key
 * is registered to the account with the password, and every money move carries
 * the id of the registered key that signed it.
 *
 *  - [register] runs right after a password sign-in (the password is in hand)
 *    and from the "confirm your password" prompt;
 *  - [deviceIdFor] tells the authorizer whether the key that is about to sign
 *    (or just signed) is the user's registered device — the key can change
 *    under the app (a biometric enrolment invalidates it; API 26–29 switches
 *    between a biometric and a screen-lock key), and a key that is not
 *    registered must never be sent as if it were;
 *  - [registrationNeeded] drives the app-wide password prompt; it is raised by
 *    [requestRegistration] (a payment found no registered key) and
 *    [onDeviceRejected] (the server refused a signature: unknown/revoked).
 *
 * Pure logic: key access, HTTP and storage are injected, so this unit-tests on
 * the JVM.
 *
 * @param publicKey The phone's current signing key (base64 SPKI DER), created
 *   if needed; null when the phone has no screen lock (no key can exist).
 */
class DeviceEnrollment(
    private val store: DeviceRegistrationStore,
    private val currentUser: () -> String?,
    private val publicKey: suspend () -> String?,
    private val label: () -> String,
    private val registerKey: suspend (RegisterDeviceRequest) -> ApiOutcome<DeviceDto>,
    private val listDevices: suspend () -> ApiOutcome<DeviceListResponse>,
    private val revokeDevice: suspend (deviceId: String) -> ApiOutcome<DeviceDto>,
    private val io: CoroutineDispatcher = Dispatchers.IO,
) {
    private val mutex = Mutex()

    private val _needed = MutableStateFlow(false)

    /** True while the app should ask for the password to (re-)register this phone. */
    val registrationNeeded: StateFlow<Boolean> = _needed.asStateFlow()

    /**
     * The server id of [userId]'s registered device whose key is [publicKey], or
     * null — never registered, registered with another key, or unreadable.
     */
    fun deviceIdFor(userId: String, publicKey: String): String? {
        val registration = try {
            store.registration(userId)
        } catch (_: LocalStorageException) {
            null
        }
        return registration?.takeIf { it.userId == userId && it.publicKey == publicKey }?.deviceId
    }

    /** A payment found no registered key: ask for the password. */
    fun requestRegistration() {
        _needed.value = true
    }

    /** The user chose "not now" (the next payment asks again). */
    fun dismiss() {
        _needed.value = false
    }

    /** Sign-out: nothing to ask the next user. */
    fun forget() {
        _needed.value = false
    }

    /**
     * The server refused a device signature (`device_signature_invalid` /
     * `device_signature_required`): the registration this phone holds is
     * unknown, revoked or stale. Forget it and ask for the password.
     */
    suspend fun onDeviceRejected(userId: String) {
        withContext(io) {
            try {
                store.clearRegistration(userId)
            } catch (_: LocalStorageException) {
                // The prompt below re-registers either way.
            }
        }
        _needed.value = true
    }

    /**
     * Register this phone's key for the signed-in user. Idempotent server-side
     * (an already-registered key answers with its existing id). With
     * [replaceOldest], first revoke the user's least recently used OTHER active
     * device (only offered after [DeviceRegistrationResult.LimitReached], i.e.
     * after the server accepted the password).
     */
    suspend fun register(password: String, replaceOldest: Boolean = false): DeviceRegistrationResult = mutex.withLock {
        val user = try {
            currentUser()?.takeIf { it.isNotBlank() }
        } catch (_: LocalStorageException) {
            null
        } ?: return DeviceRegistrationResult.Failed(ErrorCode.UNAUTHORIZED)
        val key = publicKey() ?: return DeviceRegistrationResult.NoDeviceLock
        if (replaceOldest) revokeLeastRecentlyUsed(key)?.let { return it }
        val result = when (val outcome = registerKey(RegisterDeviceRequest(key, label(), password))) {
            is ApiOutcome.Ok -> save(DeviceRegistration(user, outcome.value.id, key))
            is ApiOutcome.Failed -> when {
                outcome.coded && outcome.code == ErrorCode.FORBIDDEN -> DeviceRegistrationResult.WrongPassword
                outcome.coded && outcome.code == ErrorCode.CONFLICT -> DeviceRegistrationResult.LimitReached
                else -> DeviceRegistrationResult.Failed(outcome.code)
            }
            is ApiOutcome.Offline -> DeviceRegistrationResult.Failed(ErrorCode.UNKNOWN, offline = true)
        }
        if (result == DeviceRegistrationResult.Registered) _needed.value = false
        result
    }

    private suspend fun save(registration: DeviceRegistration): DeviceRegistrationResult = withContext(io) {
        try {
            store.saveRegistration(registration)
            DeviceRegistrationResult.Registered
        } catch (_: LocalStorageException) {
            DeviceRegistrationResult.StorageUnavailable
        }
    }

    /** Null when a slot was freed (or none needed freeing); otherwise why not. */
    private suspend fun revokeLeastRecentlyUsed(key: String): DeviceRegistrationResult? {
        val devices = when (val listed = listDevices()) {
            is ApiOutcome.Ok -> listed.value.items
            is ApiOutcome.Failed -> return DeviceRegistrationResult.Failed(listed.code)
            is ApiOutcome.Offline -> return DeviceRegistrationResult.Failed(ErrorCode.UNKNOWN, offline = true)
        }
        // RFC 3339 UTC timestamps order as strings; never used sorts first.
        val victim = devices
            .filter { it.isActive && it.publicKey != key }
            .minWithOrNull(compareBy<DeviceDto>({ it.lastUsedAt.orEmpty() }, { it.createdAt }))
            ?: return null
        return when (val revoked = revokeDevice(victim.id)) {
            is ApiOutcome.Ok -> null
            is ApiOutcome.Failed -> DeviceRegistrationResult.Failed(revoked.code)
            is ApiOutcome.Offline -> DeviceRegistrationResult.Failed(ErrorCode.UNKNOWN, offline = true)
        }
    }
}
