package tj.payment.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.fragment.app.FragmentActivity
import kotlinx.coroutines.launch
import tj.payment.wallet.security.DeviceAuthorizer

/**
 * The app lock: an opaque screen over everything (balances included) until the
 * device's own authentication succeeds. Prompts once on appearance; "Unlock"
 * prompts again. A phone with no screen lock at all cannot be locked, so it is
 * let through (money moves still refuse on such a phone).
 */
@Composable
fun LockScreen(
    authorizer: DeviceAuthorizer,
    onUnlocked: () -> Unit,
    onSignOut: () -> Unit,
) {
    val activity = LocalContext.current as? FragmentActivity
    val scope = rememberCoroutineScope()
    var message by remember { mutableStateOf<String?>(null) }
    var prompting by remember { mutableStateOf(false) }

    val text = DeviceAuthorizer.PromptText(
        title = "Unlock your wallet",
        subtitle = "Confirm it's you",
        cancel = "Cancel",
    )

    fun tryUnlock() {
        val host = activity ?: return
        if (prompting) return
        prompting = true
        message = null
        scope.launch {
            when (authorizer.unlock(host, text)) {
                DeviceAuthorizer.UnlockResult.UNLOCKED, DeviceAuthorizer.UnlockResult.NO_DEVICE_LOCK -> onUnlocked()
                DeviceAuthorizer.UnlockResult.CANCELLED -> Unit
                DeviceAuthorizer.UnlockResult.FAILED -> message = "Couldn't confirm it's you. Try again."
            }
            prompting = false
        }
    }

    LaunchedEffect(Unit) { tryUnlock() }

    Surface(
        modifier = Modifier
            .fillMaxSize()
            // Swallow every touch: nothing underneath is reachable while locked.
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {},
        color = MaterialTheme.colorScheme.background,
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(horizontal = 32.dp),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            BrandMark()
            Spacer(Modifier.height(24.dp))
            Text(
                "Wallet locked",
                style = MaterialTheme.typography.headlineMedium,
                color = MaterialTheme.colorScheme.onBackground,
            )
            Spacer(Modifier.height(8.dp))
            Text(
                "Unlock with your fingerprint or screen lock.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
            )
            message?.let {
                Spacer(Modifier.height(8.dp))
                Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium)
            }
            Spacer(Modifier.height(28.dp))
            PrimaryButton(
                text = "Unlock",
                onClick = { tryUnlock() },
                loading = prompting,
                modifier = Modifier.fillMaxWidth(),
            )
            TextButton(onClick = onSignOut) {
                Text("Sign out", color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}
