package tj.payment.wallet.security

import android.app.KeyguardManager
import android.content.Context
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.security.keystore.UserNotAuthenticatedException
import android.util.Base64
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_WEAK
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import androidx.lifecycle.Lifecycle
import java.lang.ref.WeakReference
import java.security.GeneralSecurityException
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.ProviderException
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import kotlin.coroutines.resume
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import tj.payment.core.AuthDenial
import tj.payment.core.Authorization
import tj.payment.core.PaymentAuthorizer
import tj.payment.core.PendingPayment
import tj.payment.core.authorizationPayload

/**
 * Strong device authentication for every money move (Send, Pay-by-QR, FX),
 * bound to the Android Keystore — not just a UI prompt.
 *
 * A per-install EC P-256 signing key is created with
 * `setUserAuthenticationRequired(true)`. Approving a payment means the
 * platform prompt (strongest-class biometric, or the screen-lock
 * PIN/pattern/password) unlocks that key through a [BiometricPrompt.CryptoObject],
 * and the key signs the payment's [authorizationPayload] (owner, key, accounts,
 * amount, currency). The signature is stored with the pending record. Skipping,
 * spoofing or hooking the prompt UI yields no signature — the Keystore only
 * signs after a real authentication — and the submitter never sends without one.
 *
 * Remaining step (server side, not in this app yet): register the public key
 * per device at enrolment and verify the signature on money-moving requests;
 * until then the binding is enforced on the device only.
 *
 * Modes, by what the platform supports:
 *  - API 30+: one per-use key, unlockable by BIOMETRIC_STRONG **or**
 *    DEVICE_CREDENTIAL, always via CryptoObject.
 *  - API 26–29 with a strong biometric enrolled: a per-use biometric key via
 *    CryptoObject (crypto + device credential is unsupported before API 30).
 *  - API 26–29 without one but with a screen lock: the screen lock, then a key
 *    usable only for [CREDENTIAL_WINDOW_SECONDS] after it — still enforced by
 *    the Keystore (it refuses to sign without a recent strong authentication).
 *  - No strong biometric and no screen lock: refused ([AuthDenial.NOT_ENROLLED]).
 *
 * The prompt needs a resumed activity: [MainActivity] registers itself with
 * [attach]/[detach].
 */
class DeviceAuthorizer(
    context: Context,
    private val paymentPrompt: (PendingPayment) -> PromptText,
) : PaymentAuthorizer {

    data class PromptText(val title: String, val subtitle: String, val cancel: String)

    /** What the app-lock prompt said. */
    enum class UnlockResult { UNLOCKED, CANCELLED, NO_DEVICE_LOCK, FAILED }

    private val appContext = context.applicationContext

    @Volatile
    private var host: WeakReference<FragmentActivity>? = null

    fun attach(activity: FragmentActivity) {
        host = WeakReference(activity)
    }

    fun detach(activity: FragmentActivity) {
        if (host?.get() === activity) host = null
    }

    override suspend fun authorize(payment: PendingPayment): Authorization = withContext(Dispatchers.Main) {
        val activity = resumedHost() ?: return@withContext Authorization.Denied(AuthDenial.NO_SCREEN)
        val mode = when (val chosen = chooseMode(activity)) {
            is Choice.Use -> chosen.mode
            is Choice.Refuse -> return@withContext Authorization.Denied(chosen.reason)
        }
        withTimeoutOrNull(PROMPT_TIMEOUT_MS) {
            sign(activity, mode, payment.authorizationPayload(), paymentPrompt(payment))
        } ?: Authorization.Cancelled
    }

    /**
     * The app-lock prompt: the device's own authentication, no key involved
     * (the money-moving binding is [authorize]). A phone with no screen lock at
     * all cannot be locked — reported as [UnlockResult.NO_DEVICE_LOCK].
     */
    suspend fun unlock(activity: FragmentActivity, text: PromptText): UnlockResult = withContext(Dispatchers.Main) {
        val authenticators = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            BIOMETRIC_STRONG or DEVICE_CREDENTIAL
        } else {
            BIOMETRIC_WEAK or DEVICE_CREDENTIAL
        }
        when (BiometricManager.from(activity).canAuthenticate(authenticators)) {
            BiometricManager.BIOMETRIC_SUCCESS -> Unit
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> return@withContext UnlockResult.NO_DEVICE_LOCK
            else -> return@withContext UnlockResult.FAILED
        }
        if (activity.supportFragmentManager.isStateSaved) return@withContext UnlockResult.FAILED
        when (prompt(activity, text, authenticators, crypto = null)) {
            is PromptResult.Success -> UnlockResult.UNLOCKED
            PromptResult.Cancelled -> UnlockResult.CANCELLED
            is PromptResult.Error -> UnlockResult.FAILED
        }
    }

    // --- mode selection ---

    private enum class Mode { STRONG_OR_CREDENTIAL, STRONG_BIOMETRIC, CREDENTIAL_WINDOW }

    private sealed interface Choice {
        data class Use(val mode: Mode) : Choice
        data class Refuse(val reason: AuthDenial) : Choice
    }

    private fun chooseMode(context: Context): Choice {
        val biometrics = BiometricManager.from(context)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val status = biometrics.canAuthenticate(BIOMETRIC_STRONG or DEVICE_CREDENTIAL)
            return if (status == BiometricManager.BIOMETRIC_SUCCESS) Choice.Use(Mode.STRONG_OR_CREDENTIAL) else Choice.Refuse(denial(status))
        }
        if (biometrics.canAuthenticate(BIOMETRIC_STRONG) == BiometricManager.BIOMETRIC_SUCCESS) {
            return Choice.Use(Mode.STRONG_BIOMETRIC)
        }
        val keyguard = context.getSystemService(KeyguardManager::class.java)
        return if (keyguard?.isDeviceSecure == true) Choice.Use(Mode.CREDENTIAL_WINDOW) else Choice.Refuse(AuthDenial.NOT_ENROLLED)
    }

    private fun denial(status: Int): AuthDenial = when (status) {
        BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> AuthDenial.NOT_ENROLLED
        BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED -> AuthDenial.SECURITY_UPDATE_REQUIRED
        else -> AuthDenial.UNAVAILABLE
    }

    // --- signing ---

    private suspend fun sign(activity: FragmentActivity, mode: Mode, payload: ByteArray, text: PromptText): Authorization {
        if (activity.supportFragmentManager.isStateSaved) return Authorization.Denied(AuthDenial.NO_SCREEN)
        return when (mode) {
            Mode.STRONG_OR_CREDENTIAL, Mode.STRONG_BIOMETRIC -> {
                val signature = try {
                    initSignature(mode)
                } catch (e: GeneralSecurityException) {
                    return Authorization.Denied(AuthDenial.UNAVAILABLE)
                } catch (e: ProviderException) {
                    return Authorization.Denied(AuthDenial.UNAVAILABLE)
                }
                val authenticators = if (mode == Mode.STRONG_OR_CREDENTIAL) BIOMETRIC_STRONG or DEVICE_CREDENTIAL else BIOMETRIC_STRONG
                when (val result = prompt(activity, text, authenticators, BiometricPrompt.CryptoObject(signature))) {
                    is PromptResult.Success -> {
                        // Only the CryptoObject the prompt authorized can sign.
                        val unlocked = result.result.cryptoObject?.signature
                            ?: return Authorization.Denied(AuthDenial.FAILED)
                        signWith(unlocked, payload)
                    }
                    PromptResult.Cancelled -> Authorization.Cancelled
                    is PromptResult.Error -> Authorization.Denied(result.reason)
                }
            }
            Mode.CREDENTIAL_WINDOW ->
                when (val result = prompt(activity, text, BIOMETRIC_WEAK or DEVICE_CREDENTIAL, crypto = null)) {
                    // The key itself refuses unless a strong authentication
                    // (the screen lock) happened in the last few seconds.
                    is PromptResult.Success -> try {
                        signWith(initSignature(mode), payload)
                    } catch (e: GeneralSecurityException) {
                        Authorization.Denied(AuthDenial.FAILED)
                    } catch (e: ProviderException) {
                        Authorization.Denied(AuthDenial.FAILED)
                    }
                    PromptResult.Cancelled -> Authorization.Cancelled
                    is PromptResult.Error -> Authorization.Denied(result.reason)
                }
        }
    }

    private fun signWith(signature: Signature, payload: ByteArray): Authorization = try {
        signature.update(payload)
        Authorization.Granted(Base64.encodeToString(signature.sign(), Base64.NO_WRAP))
    } catch (e: UserNotAuthenticatedException) {
        Authorization.Denied(AuthDenial.FAILED)
    } catch (e: GeneralSecurityException) {
        Authorization.Denied(AuthDenial.FAILED)
    } catch (e: ProviderException) {
        Authorization.Denied(AuthDenial.FAILED)
    }

    /**
     * A Signature initialized with the mode's key, creating it on first use.
     * A key invalidated by a new biometric enrolment or a removed screen lock
     * is replaced (no server binding exists yet; once it does, a replaced key
     * must be re-registered after a password sign-in).
     */
    private fun initSignature(mode: Mode): Signature {
        val alias = aliasFor(mode)
        return try {
            Signature.getInstance(SIGNATURE_ALGORITHM).apply { initSign(privateKey(alias) ?: generate(alias, mode)) }
        } catch (e: KeyPermanentlyInvalidatedException) {
            keyStore().deleteEntry(alias)
            Signature.getInstance(SIGNATURE_ALGORITHM).apply { initSign(generate(alias, mode)) }
        }
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }

    private fun privateKey(alias: String): PrivateKey? = keyStore().getKey(alias, null) as? PrivateKey

    @Suppress("DEPRECATION") // setUserAuthenticationValidityDurationSeconds: the API < 30 path only
    private fun generate(alias: String, mode: Mode): PrivateKey {
        val builder = KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_SIGN)
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256)
            .setUserAuthenticationRequired(true)
        if (mode == Mode.STRONG_OR_CREDENTIAL && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Per use (timeout 0): every signature needs its own authentication.
            builder.setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG or KeyProperties.AUTH_DEVICE_CREDENTIAL)
        } else if (mode == Mode.STRONG_BIOMETRIC) {
            // Per-use biometric is the default for an auth-required key without a validity window.
            builder.setInvalidatedByBiometricEnrollment(true)
        } else if (mode == Mode.CREDENTIAL_WINDOW) {
            builder.setUserAuthenticationValidityDurationSeconds(CREDENTIAL_WINDOW_SECONDS)
        }
        val spec = builder.build()
        return KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, ANDROID_KEYSTORE)
            .apply { initialize(spec) }
            .generateKeyPair()
            .private
    }

    private fun aliasFor(mode: Mode): String = when (mode) {
        Mode.STRONG_OR_CREDENTIAL -> "tj.payment.wallet.pay.v1.strong"
        Mode.STRONG_BIOMETRIC -> "tj.payment.wallet.pay.v1.biometric"
        Mode.CREDENTIAL_WINDOW -> "tj.payment.wallet.pay.v1.credential"
    }

    // --- the prompt ---

    private sealed interface PromptResult {
        data class Success(val result: BiometricPrompt.AuthenticationResult) : PromptResult
        data object Cancelled : PromptResult
        data class Error(val reason: AuthDenial) : PromptResult
    }

    private fun resumedHost(): FragmentActivity? =
        host?.get()?.takeIf { it.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED) }

    private suspend fun prompt(
        activity: FragmentActivity,
        text: PromptText,
        authenticators: Int,
        crypto: BiometricPrompt.CryptoObject?,
    ): PromptResult = suspendCancellableCoroutine { cont ->
        val executor = ContextCompat.getMainExecutor(activity)
        val prompt = BiometricPrompt(
            activity,
            executor,
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    if (cont.isActive) cont.resume(PromptResult.Success(result))
                }

                override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                    if (cont.isActive) cont.resume(errorResult(errorCode))
                }

                // A single non-matching finger: the prompt stays up and lets
                // the user try again; only onAuthenticationError ends it.
                override fun onAuthenticationFailed() = Unit
            },
        )
        val info = BiometricPrompt.PromptInfo.Builder()
            .setTitle(text.title)
            .setSubtitle(text.subtitle)
            .setAllowedAuthenticators(authenticators)
            .apply { if (authenticators and DEVICE_CREDENTIAL == 0) setNegativeButtonText(text.cancel) }
            // A money move is confirmed explicitly, even with passive (face) biometrics.
            .setConfirmationRequired(true)
            .build()
        if (crypto != null) prompt.authenticate(info, crypto) else prompt.authenticate(info)
        cont.invokeOnCancellation { executor.execute { prompt.cancelAuthentication() } }
    }

    private fun errorResult(errorCode: Int): PromptResult = when (errorCode) {
        BiometricPrompt.ERROR_USER_CANCELED,
        BiometricPrompt.ERROR_NEGATIVE_BUTTON,
        BiometricPrompt.ERROR_CANCELED,
        BiometricPrompt.ERROR_TIMEOUT,
        -> PromptResult.Cancelled
        BiometricPrompt.ERROR_LOCKOUT, BiometricPrompt.ERROR_LOCKOUT_PERMANENT -> PromptResult.Error(AuthDenial.LOCKED_OUT)
        BiometricPrompt.ERROR_NO_BIOMETRICS, BiometricPrompt.ERROR_NO_DEVICE_CREDENTIAL -> PromptResult.Error(AuthDenial.NOT_ENROLLED)
        BiometricPrompt.ERROR_HW_NOT_PRESENT, BiometricPrompt.ERROR_HW_UNAVAILABLE -> PromptResult.Error(AuthDenial.UNAVAILABLE)
        else -> PromptResult.Error(AuthDenial.FAILED)
    }

    private companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val SIGNATURE_ALGORITHM = "SHA256withECDSA"

        /** API < 30 fallback: how long after the screen lock the key may sign. */
        const val CREDENTIAL_WINDOW_SECONDS = 10

        /** A prompt nobody answers is treated as dismissed (and releases the payment slot). */
        const val PROMPT_TIMEOUT_MS = 3 * 60_000L
    }
}
