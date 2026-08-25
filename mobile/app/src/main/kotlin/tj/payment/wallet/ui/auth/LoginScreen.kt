package tj.payment.wallet.ui.auth

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.slideInVertically
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tj.payment.wallet.ui.BrandMark
import tj.payment.wallet.ui.PrimaryButton
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.Rust

@Composable
fun LoginScreen(
    viewModel: AuthViewModel,
    onAuthenticated: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    if (state.authenticated) {
        onAuthenticated()
        return
    }

    val isRegister = state.mode == AuthMode.REGISTER

    // One-shot entrance: content fades and eases up on first composition.
    val entered = remember { MutableTransitionState(false).apply { targetState = true } }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .imePadding()
            .padding(horizontal = 24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        AnimatedVisibility(
            visibleState = entered,
            enter = fadeIn(tween(450)) + slideInVertically(tween(450)) { it / 6 },
        ) {
            Column {
                BrandMark(size = 60)

                Spacer(Modifier.height(24.dp))

                Text(
                    text = if (isRegister) "Create your wallet" else "Welcome back",
                    style = MaterialTheme.typography.headlineMedium,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    text = if (isRegister) {
                        "Register with your phone number."
                    } else {
                        "Sign in to continue."
                    },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )

                Spacer(Modifier.height(28.dp))

                OutlinedTextField(
                    value = state.phone,
                    onValueChange = viewModel::onPhoneChange,
                    label = { Text("Phone number") },
                    singleLine = true,
                    shape = RoundedCornerShape(14.dp),
                    colors = fieldColors(),
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Phone,
                        imeAction = ImeAction.Next,
                    ),
                    modifier = Modifier.fillMaxWidth(),
                )

                Spacer(Modifier.height(14.dp))

                OutlinedTextField(
                    value = state.password,
                    onValueChange = viewModel::onPasswordChange,
                    label = { Text("Password") },
                    singleLine = true,
                    shape = RoundedCornerShape(14.dp),
                    colors = fieldColors(),
                    visualTransformation = PasswordVisualTransformation(),
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Password,
                        imeAction = ImeAction.Done,
                    ),
                    modifier = Modifier.fillMaxWidth(),
                )

                AnimatedVisibility(
                    visible = state.error != null,
                    enter = fadeIn() + expandVertically(),
                    exit = fadeOut() + shrinkVertically(),
                ) {
                    Text(
                        text = state.error ?: "",
                        color = NegativeRed,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.padding(top = 12.dp),
                    )
                }

                Spacer(Modifier.height(24.dp))

                PrimaryButton(
                    text = if (isRegister) "Create wallet" else "Sign in",
                    onClick = viewModel::submit,
                    enabled = state.canSubmit,
                    loading = state.submitting,
                    modifier = Modifier.fillMaxWidth(),
                )

                Spacer(Modifier.height(6.dp))

                TextButton(
                    onClick = viewModel::toggleMode,
                    enabled = !state.submitting,
                    modifier = Modifier.align(Alignment.CenterHorizontally),
                ) {
                    Text(
                        if (isRegister) "Have an account? Sign in" else "New here? Create a wallet",
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

@Composable
private fun fieldColors() = OutlinedTextFieldDefaults.colors(
    focusedBorderColor = Rust,
    focusedLabelColor = Rust,
    cursorColor = Rust,
)
