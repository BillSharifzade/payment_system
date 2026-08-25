package tj.payment.wallet

import android.content.Context
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

    val session: SecureSession by lazy { SecureSession(appContext) }
    private val api: ApiClient by lazy { ApiClient(BuildConfig.API_BASE_URL, session) }

    val authRepository: AuthRepository by lazy { AuthRepository(api, session) }
    val walletRepository: WalletRepository by lazy {
        WalletRepository(api, PendingPaymentPrefsStore(appContext))
    }
}
