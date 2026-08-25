package tj.payment.wallet.ui

import androidx.compose.animation.AnimatedContentTransitionScope
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import tj.payment.core.ApiOutcome
import tj.payment.wallet.AppContainer
import tj.payment.wallet.data.AuthRepository
import tj.payment.wallet.ui.auth.AuthViewModel
import tj.payment.wallet.ui.auth.LoginScreen
import tj.payment.wallet.ui.fx.FxScreen
import tj.payment.wallet.ui.fx.FxViewModel
import tj.payment.wallet.ui.history.HistoryScreen
import tj.payment.wallet.ui.history.HistoryViewModel
import tj.payment.wallet.ui.home.HomeScreen
import tj.payment.wallet.ui.home.HomeViewModel
import tj.payment.wallet.ui.kyc.KycScreen
import tj.payment.wallet.ui.kyc.KycViewModel
import tj.payment.wallet.ui.receive.ReceiveScreen
import tj.payment.wallet.ui.send.SendScreen
import tj.payment.wallet.ui.send.SendViewModel

private object Routes {
    const val GATE = "gate"
    const val LOGIN = "login"
    const val HOME = "home"
    const val SEND = "send"
    const val RECEIVE = "receive"
    const val HISTORY = "history/{accountId}"
    const val KYC = "kyc"
    const val FX = "fx"

    fun history(accountId: String) = "history/$accountId"
}

@Composable
fun AppRoot(container: AppContainer) {
    val nav = rememberNavController()

    val slideDuration = 300
    NavHost(
        navController = nav,
        startDestination = Routes.GATE,
        enterTransition = {
            slideIntoContainer(
                AnimatedContentTransitionScope.SlideDirection.Start,
                animationSpec = tween(slideDuration),
            ) + fadeIn(tween(slideDuration))
        },
        exitTransition = { fadeOut(tween(slideDuration / 2)) },
        popEnterTransition = { fadeIn(tween(slideDuration)) },
        popExitTransition = {
            slideOutOfContainer(
                AnimatedContentTransitionScope.SlideDirection.End,
                animationSpec = tween(slideDuration),
            ) + fadeOut(tween(slideDuration))
        },
    ) {
        composable(Routes.GATE) {
            GateScreen(
                repo = container.authRepository,
                toHome = {
                    nav.navigate(Routes.HOME) { popUpTo(Routes.GATE) { inclusive = true } }
                },
                toLogin = {
                    nav.navigate(Routes.LOGIN) { popUpTo(Routes.GATE) { inclusive = true } }
                },
            )
        }
        composable(Routes.LOGIN) {
            val vm = viewModel { AuthViewModel(container.authRepository) }
            LoginScreen(vm, onAuthenticated = {
                nav.navigate(Routes.HOME) { popUpTo(Routes.LOGIN) { inclusive = true } }
            })
        }
        composable(Routes.HOME) {
            val vm = viewModel {
                HomeViewModel(container.authRepository, container.walletRepository)
            }
            // Coming back from Send/KYC/FX must show fresh balances & activity.
            LaunchedEffect(Unit) { vm.refresh() }
            HomeScreen(
                viewModel = vm,
                onLoggedOut = {
                    nav.navigate(Routes.LOGIN) { popUpTo(Routes.HOME) { inclusive = true } }
                },
                onSend = { nav.navigate(Routes.SEND) },
                onReceive = { nav.navigate(Routes.RECEIVE) },
                onHistory = { walletId -> nav.navigate(Routes.history(walletId)) },
                onConvert = { nav.navigate(Routes.FX) },
                onKyc = { nav.navigate(Routes.KYC) },
            )
        }
        composable(Routes.SEND) {
            val vm = viewModel { SendViewModel(container.walletRepository) }
            SendScreen(vm, onClose = { nav.popBackStack() })
        }
        composable(Routes.RECEIVE) {
            ReceiveScreen(
                phone = container.session.phone(),
                onBack = { nav.popBackStack() },
            )
        }
        composable(Routes.HISTORY) { backStack ->
            val accountId = backStack.arguments?.getString("accountId").orEmpty()
            val vm = viewModel {
                HistoryViewModel(container.walletRepository, accountId)
            }
            HistoryScreen(vm, onBack = { nav.popBackStack() })
        }
        composable(Routes.KYC) {
            val vm = viewModel { KycViewModel(container.walletRepository) }
            KycScreen(vm, onBack = { nav.popBackStack() })
        }
        composable(Routes.FX) {
            val vm = viewModel { FxViewModel(container.walletRepository) }
            FxScreen(vm, onBack = { nav.popBackStack() })
        }
    }
}

/** Launch splash: restore a persisted session, then route to home or login. */
@Composable
private fun GateScreen(
    repo: AuthRepository,
    toHome: () -> Unit,
    toLogin: () -> Unit,
) {
    LaunchedEffect(Unit) {
        if (!repo.hasPersistedSession()) {
            toLogin()
            return@LaunchedEffect
        }
        when (repo.restore()) {
            is ApiOutcome.Ok -> toHome()
            else -> toLogin()
        }
    }

    Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        CircularProgressIndicator(color = MaterialTheme.colorScheme.primary)
    }
}
