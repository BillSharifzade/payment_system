package tj.payment.wallet

import android.content.Context
import android.os.SystemClock
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.core.AppLockPolicy
import tj.payment.core.Currency
import tj.payment.core.Money
import tj.payment.core.PaymentKind
import tj.payment.wallet.data.ApiClient
import tj.payment.wallet.data.AuthRepository
import tj.payment.wallet.data.CertificatePins
import tj.payment.wallet.data.PendingPaymentPrefsStore
import tj.payment.wallet.data.SecureSession
import tj.payment.wallet.data.WalletRepository
import tj.payment.wallet.security.DeviceAuthorizer

/**
 * Manual dependency graph — deliberately no Hilt/Dagger. For an app this size,
 * plain constructor wiring is faster to build, adds no annotation-processing
 * step, and keeps the dependency chain readable in one place. Widen this as the
 * app grows (repositories per feature); swap in a DI framework only if it earns
 * its keep.
 */
class AppContainer(context: Context) {
    private val appContext = context.applicationContext

    /** Process-wide work that outlives any screen (store warm-up, token rotation). */
    private val appScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private val _foreground = MutableStateFlow(false)

    /** True while the activity is started; driven by [PaymentApp]. */
    val foreground: StateFlow<Boolean> = _foreground.asStateFlow()

    /**
     * The app lock (FRONTEND.md §2.3). Monotonic clock: changing the wall clock
     * while the app is in the background cannot skip the lock.
     */
    val appLock = AppLockPolicy(clock = SystemClock::elapsedRealtime)

    fun setForeground(value: Boolean) {
        _foreground.value = value
        if (value) appLock.onForeground { session.hasPersistedSession() } else appLock.onBackground()
    }

    val session: SecureSession by lazy { SecureSession(appContext) }
    val pendingStore: PendingPaymentPrefsStore by lazy { PendingPaymentPrefsStore(appContext) }

    /** Strong device authentication for every money move; MainActivity is its prompt host. */
    val authorizer: DeviceAuthorizer by lazy {
        DeviceAuthorizer(appContext) { payment ->
            val amount = Money.ofMinor(payment.amountMinor, Currency.of(payment.currency)).format()
            val res = appContext.resources
            DeviceAuthorizer.PromptText(
                title = res.getString(R.string.auth_prompt_title),
                subtitle = when (payment.kind) {
                    PaymentKind.FX -> res.getString(R.string.auth_prompt_fx, amount, payment.recipientLabel)
                    PaymentKind.TRANSFER, PaymentKind.CHECK -> res.getString(R.string.auth_prompt_pay, amount, payment.recipientLabel)
                },
                cancel = res.getString(R.string.action_cancel),
            )
        }
    }

    private val api: ApiClient by lazy {
        val pins = CertificatePins.parse(BuildConfig.CERT_PINS)
        // Defence in depth behind the build-time gate: a prod release never
        // talks to the API unpinned. (Debug builds and dev/staging are exempt.)
        check(!(BuildConfig.REQUIRE_CERT_PINS && !BuildConfig.DEBUG && pins.isEmpty())) {
            "prod release built without certificate pins (-PpaymentCertPins)"
        }
        ApiClient(
            BuildConfig.API_BASE_URL,
            session,
            foreground = foreground,
            scope = appScope,
            certificatePins = pins,
        )
    }

    val authRepository: AuthRepository by lazy { AuthRepository(api, session) }
    val walletRepository: WalletRepository by lazy {
        WalletRepository(
            api = api,
            pendingStore = pendingStore,
            currentUser = { session.userId() },
            authorizer = authorizer,
        )
    }

    /**
     * Open both Keystore-backed stores off the main thread. First touch costs a
     * Keystore round trip plus Tink keyset decryption (tens to hundreds of ms on
     * low-end phones); doing it here, during Application.onCreate, means the
     * first screen never pays it on the main thread. `lazy` is synchronized, so
     * a main-thread reader that arrives early simply waits for this to finish.
     */
    fun warmUp() {
        appScope.launch(Dispatchers.IO) {
            session
            pendingStore
        }
    }
}
