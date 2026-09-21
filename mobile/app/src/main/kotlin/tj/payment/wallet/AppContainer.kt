package tj.payment.wallet

import android.content.Context
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import tj.payment.wallet.data.ApiClient
import tj.payment.wallet.data.AuthRepository
import tj.payment.wallet.data.PendingPaymentPrefsStore
import tj.payment.wallet.data.SecureSession
import tj.payment.wallet.data.WalletRepository

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

    fun setForeground(value: Boolean) {
        _foreground.value = value
    }

    val session: SecureSession by lazy { SecureSession(appContext) }
    val pendingStore: PendingPaymentPrefsStore by lazy { PendingPaymentPrefsStore(appContext) }
    private val api: ApiClient by lazy {
        ApiClient(BuildConfig.API_BASE_URL, session, foreground = foreground, scope = appScope)
    }

    val authRepository: AuthRepository by lazy { AuthRepository(api, session) }
    val walletRepository: WalletRepository by lazy { WalletRepository(api, pendingStore) }

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
