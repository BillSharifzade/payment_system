package tj.payment.core

import java.io.IOException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Registering the phone's signing key, and which device id a payment may carry. */
class DeviceEnrollmentTest {

    private class Store : DeviceRegistrationStore {
        var saved: DeviceRegistration? = null
        var faulty = false
        override fun registration(userId: String): DeviceRegistration? {
            if (faulty) throw LocalStorageException("keystore down")
            return saved?.takeIf { it.userId == userId }
        }
        override fun saveRegistration(registration: DeviceRegistration) {
            if (faulty) throw LocalStorageException("keystore down")
            saved = registration
        }
        override fun clearRegistration(userId: String) {
            if (saved?.userId == userId) saved = null
        }
    }

    private class Server {
        val registered = mutableListOf<RegisterDeviceRequest>()
        val revoked = mutableListOf<String>()
        var register: (RegisterDeviceRequest) -> ApiOutcome<DeviceDto> = { r -> ApiOutcome.Ok(device("dev-1", r.publicKey)) }
        var devices: List<DeviceDto> = emptyList()
    }

    private var user: String? = "alice"
    private var key: String? = "KEY-A"

    private fun enrollment(store: Store, server: Server) = DeviceEnrollment(
        store = store,
        currentUser = { user },
        publicKey = { key },
        label = { "Pixel 8" },
        registerKey = { r -> server.registered += r; server.register(r) },
        listDevices = { ApiOutcome.Ok(DeviceListResponse(server.devices)) },
        revokeDevice = { id -> server.revoked += id; ApiOutcome.Ok(device(id, "x", revokedAt = "2026-10-08T00:00:00.000000Z")) },
        io = Dispatchers.Unconfined,
    )

    @Test
    fun `a registration binds this user and key to the server's device id`() = runBlocking {
        val store = Store()
        val server = Server()
        val e = enrollment(store, server)
        e.requestRegistration()
        assertTrue(e.registrationNeeded.value)

        assertEquals(DeviceRegistrationResult.Registered, e.register("password123"))
        assertEquals(RegisterDeviceRequest("KEY-A", "Pixel 8", "password123"), server.registered.single())
        assertEquals(DeviceRegistration("alice", "dev-1", "KEY-A"), store.saved)
        assertFalse(e.registrationNeeded.value)

        assertEquals("dev-1", e.deviceIdFor("alice", "KEY-A"))
        assertNull("another key (e.g. regenerated) is not the registered device", e.deviceIdFor("alice", "KEY-B"))
        assertNull("another user on this phone", e.deviceIdFor("bob", "KEY-A"))
    }

    @Test
    fun `the server's refusals are told apart`() = runBlocking {
        val cases = listOf(
            ApiOutcome.Failed(ErrorCode.FORBIDDEN, 403, "incorrect password", coded = true) to DeviceRegistrationResult.WrongPassword,
            ApiOutcome.Failed(ErrorCode.CONFLICT, 409, "limit", coded = true) to DeviceRegistrationResult.LimitReached,
            ApiOutcome.Failed(ErrorCode.RATE_LIMITED, 429, "slow down", coded = true) to DeviceRegistrationResult.Failed(ErrorCode.RATE_LIMITED),
            ApiOutcome.Failed(ErrorCode.ACCOUNT_BLOCKED, 403, "blocked", coded = true) to DeviceRegistrationResult.Failed(ErrorCode.ACCOUNT_BLOCKED),
            // A bare 403 from a proxy is not a verdict on the password.
            ApiOutcome.Failed(ErrorCode.FORBIDDEN, 403, null, coded = false) to DeviceRegistrationResult.Failed(ErrorCode.FORBIDDEN),
            ApiOutcome.Offline(IOException("down")) to DeviceRegistrationResult.Failed(ErrorCode.UNKNOWN, offline = true),
        )
        for ((answer, expected) in cases) {
            val store = Store()
            val server = Server().apply { register = { answer } }
            val e = enrollment(store, server)
            e.requestRegistration()
            assertEquals("$answer", expected, e.register("pw-pw-pw-pw"))
            assertNull(store.saved)
            assertTrue("still needed after $answer", e.registrationNeeded.value)
        }
    }

    @Test
    fun `no screen lock means no key and nothing is sent`() = runBlocking {
        key = null
        val server = Server()
        assertEquals(DeviceRegistrationResult.NoDeviceLock, enrollment(Store(), server).register("password123"))
        assertTrue(server.registered.isEmpty())
    }

    @Test
    fun `signed out, nothing is registered`() = runBlocking {
        user = null
        val server = Server()
        assertEquals(DeviceRegistrationResult.Failed(ErrorCode.UNAUTHORIZED), enrollment(Store(), server).register("password123"))
        assertTrue(server.registered.isEmpty())
    }

    @Test
    fun `a registration the phone cannot store is reported, not hidden`() = runBlocking {
        val store = Store().apply { faulty = true }
        assertEquals(DeviceRegistrationResult.StorageUnavailable, enrollment(store, Server()).register("password123"))
    }

    @Test
    fun `at the device limit, replacing frees the least recently used other device`() = runBlocking {
        val server = Server().apply {
            devices = listOf(
                device("used-recently", "K1", lastUsedAt = "2026-10-07T10:00:00.000000Z", createdAt = "2026-01-01T00:00:00.000000Z"),
                device("used-long-ago", "K2", lastUsedAt = "2026-03-01T10:00:00.000000Z", createdAt = "2026-02-01T00:00:00.000000Z"),
                device("never-used", "K3", lastUsedAt = null, createdAt = "2026-09-01T00:00:00.000000Z"),
                device("already-revoked", "K4", lastUsedAt = null, createdAt = "2025-01-01T00:00:00.000000Z", revokedAt = "2025-06-01T00:00:00.000000Z"),
            )
        }
        val store = Store()
        assertEquals(DeviceRegistrationResult.Registered, enrollment(store, server).register("password123", replaceOldest = true))
        assertEquals(listOf("never-used"), server.revoked)
        assertEquals("dev-1", store.saved?.deviceId)

        server.devices = server.devices.filter { it.id != "never-used" }
        server.revoked.clear()
        enrollment(Store(), server).register("password123", replaceOldest = true)
        assertEquals(listOf("used-long-ago"), server.revoked)
    }

    @Test
    fun `a refused signature forgets the registration and asks for the password`() = runBlocking {
        val store = Store().apply { saved = DeviceRegistration("alice", "dev-1", "KEY-A") }
        val e = enrollment(store, Server())
        assertFalse(e.registrationNeeded.value)
        e.onDeviceRejected("alice")
        assertNull(store.saved)
        assertNull(e.deviceIdFor("alice", "KEY-A"))
        assertTrue(e.registrationNeeded.value)
        e.dismiss()
        assertFalse(e.registrationNeeded.value)
    }

    @Test
    fun `an unreadable store reads as not registered`() {
        val store = Store().apply { saved = DeviceRegistration("alice", "dev-1", "KEY-A"); faulty = true }
        assertNull(enrollment(store, Server()).deviceIdFor("alice", "KEY-A"))
    }

    private companion object {
        fun device(
            id: String,
            publicKey: String,
            lastUsedAt: String? = null,
            createdAt: String = "2026-10-08T00:00:00.000000Z",
            revokedAt: String? = null,
        ) = DeviceDto(id, "Phone", publicKey, createdAt, lastUsedAt, revokedAt)
    }
}
