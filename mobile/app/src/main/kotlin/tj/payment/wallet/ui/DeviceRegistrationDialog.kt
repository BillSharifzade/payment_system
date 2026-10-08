package tj.payment.wallet.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import tj.payment.core.AuthDenial
import tj.payment.core.DeviceEnrollment
import tj.payment.core.DeviceRegistrationResult
import tj.payment.wallet.R

/**
 * The app-wide "confirm your password" prompt of device binding: shown while
 * [DeviceEnrollment.registrationNeeded] — a payment found this phone's key
 * unregistered, or the server refused its signature. Registering needs the
 * password (a stolen session alone cannot add a phone). At the device limit it
 * offers to replace the least recently used phone. The payment itself is not
 * resumed: the user confirms it again once the phone is linked.
 *
 * The password lives only in this composable's memory (never saved state).
 */
@Composable
fun DeviceRegistrationDialog(enrollment: DeviceEnrollment) {
    val scope = rememberCoroutineScope()
    var password by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    var limitReached by remember { mutableStateOf(false) }

    fun submit() {
        if (busy || password.isEmpty()) return
        busy = true
        message = null
        val replace = limitReached
        scope.launch {
            val result = enrollment.register(password, replaceOldest = replace)
            busy = false
            // Registered: registrationNeeded drops and AppRoot removes this dialog.
            if (result == DeviceRegistrationResult.Registered) password = ""
            if (result == DeviceRegistrationResult.LimitReached) limitReached = true
            message = when (result) {
                DeviceRegistrationResult.Registered -> null
                DeviceRegistrationResult.WrongPassword -> Copy.text(R.string.device_wrong_password)
                DeviceRegistrationResult.LimitReached -> Copy.text(R.string.device_limit_reached)
                DeviceRegistrationResult.NoDeviceLock -> AuthDenial.NOT_ENROLLED.userMessage()
                DeviceRegistrationResult.StorageUnavailable -> STORAGE_MESSAGE
                is DeviceRegistrationResult.Failed ->
                    if (result.offline) OFFLINE_MESSAGE else result.code.userMessage()
            }
        }
    }

    AlertDialog(
        onDismissRequest = { if (!busy) enrollment.dismiss() },
        title = { Text(stringResource(R.string.device_register_title)) },
        text = {
            Column {
                Text(
                    text = stringResource(R.string.device_register_body),
                    style = MaterialTheme.typography.bodyMedium,
                )
                Spacer(Modifier.height(16.dp))
                OutlinedTextField(
                    value = password,
                    onValueChange = {
                        password = it
                        if (!limitReached) message = null
                    },
                    label = { Text(stringResource(R.string.login_password)) },
                    singleLine = true,
                    enabled = !busy,
                    visualTransformation = PasswordVisualTransformation(),
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Password,
                        imeAction = ImeAction.Done,
                    ),
                    modifier = Modifier.fillMaxWidth(),
                )
                val shown = message
                if (shown != null) {
                    Text(
                        text = shown,
                        color = MaterialTheme.colorScheme.error,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.padding(top = 12.dp),
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { submit() }, enabled = !busy && password.isNotEmpty()) {
                Text(
                    stringResource(
                        if (limitReached) R.string.device_register_replace else R.string.device_register_confirm,
                    ),
                )
            }
        },
        dismissButton = {
            TextButton(onClick = { enrollment.dismiss() }, enabled = !busy) {
                Text(stringResource(R.string.device_register_later))
            }
        },
    )
}
