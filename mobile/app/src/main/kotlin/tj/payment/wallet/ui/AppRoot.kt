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
import tj.payment.wallet.ui.home.HomeScreen
import tj.payment.wallet.ui.home.HomeViewModel

private object Routes {
    const val GATE = "gate"
    const val LOGIN = "login"
    const val HOME = "home"
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
            val vm = viewModel { HomeViewModel(container.authRepository) }
            HomeScreen(vm, onLoggedOut = {
                nav.navigate(Routes.LOGIN) { popUpTo(Routes.HOME) { inclusive = true } }
            })
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
