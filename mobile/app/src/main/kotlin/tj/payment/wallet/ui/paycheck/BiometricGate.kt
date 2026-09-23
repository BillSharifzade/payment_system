package tj.payment.wallet.ui.paycheck

import android.os.Build
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_WEAK
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine

/** What the device's own authentication said about the person holding it. */
sealed interface GateResult {
    /** The phone's owner confirmed (fingerprint, face, or the device PIN/pattern). */
    data object Confirmed : GateResult

    /** The prompt was dismissed or timed out — nothing was sent. */
    data object Cancelled : GateResult

    /**
     * No fingerprint, face or screen lock is set up, or the hardware is
     * unavailable. Paying is refused: the device offers nothing to bind the
     * payment to. [reason] is user-facing copy.
     */
    data class Unavailable(val reason: String) : GateResult

    data class Failed(val reason: String) : GateResult
}

/**
 * The device biometric gate in front of a check payment. A phone's sensor can
 * only ever answer "is this the phone's owner?" — the *who* is the signed-in
 * user — which is exactly the question here: this person is already
 * identified by their session; the fingerprint proves it is them holding the
 * phone at the moment of paying.
 *
 * Strong biometrics with the device credential as fallback (API 30+); on 28–29
 * the strong+credential combination is not supported, so weak biometrics +
 * credential is used there.
 */
object BiometricGate {
    private fun authenticators(): Int =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            BIOMETRIC_STRONG or DEVICE_CREDENTIAL
        } else {
            BIOMETRIC_WEAK or DEVICE_CREDENTIAL
        }

    /** Null when the device can prompt; otherwise why it can't (user-facing). */
    fun unavailableReason(activity: FragmentActivity): String? =
        when (BiometricManager.from(activity).canAuthenticate(authenticators())) {
            BiometricManager.BIOMETRIC_SUCCESS -> null
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED ->
                "Set up a fingerprint or a screen lock in your phone's settings to pay."
            BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE,
            BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE ->
                "This phone can't confirm it's you (no fingerprint sensor or screen lock available)."
            BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED ->
                "Your phone needs a security update before it can confirm payments."
            else -> "This phone can't confirm payments right now."
        }

    suspend fun confirm(activity: FragmentActivity, title: String, subtitle: String): GateResult {
        unavailableReason(activity)?.let { return GateResult.Unavailable(it) }
        return suspendCancellableCoroutine { cont ->
            val prompt = BiometricPrompt(
                activity,
                ContextCompat.getMainExecutor(activity),
                object : BiometricPrompt.AuthenticationCallback() {
                    override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                        if (cont.isActive) cont.resume(GateResult.Confirmed)
                    }

                    override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                        if (!cont.isActive) return
                        val cancelled = errorCode == BiometricPrompt.ERROR_USER_CANCELED ||
                            errorCode == BiometricPrompt.ERROR_NEGATIVE_BUTTON ||
                            errorCode == BiometricPrompt.ERROR_CANCELED ||
                            errorCode == BiometricPrompt.ERROR_TIMEOUT
                        cont.resume(if (cancelled) GateResult.Cancelled else GateResult.Failed(errString.toString()))
                    }

                    // A single non-matching finger: the prompt stays up and lets
                    // the user try again; only onAuthenticationError ends it.
                    override fun onAuthenticationFailed() = Unit
                },
            )
            val info = BiometricPrompt.PromptInfo.Builder()
                .setTitle(title)
                .setSubtitle(subtitle)
                .setAllowedAuthenticators(authenticators())
                .setConfirmationRequired(false)
                .build()
            prompt.authenticate(info)
            cont.invokeOnCancellation { prompt.cancelAuthentication() }
        }
    }
}
