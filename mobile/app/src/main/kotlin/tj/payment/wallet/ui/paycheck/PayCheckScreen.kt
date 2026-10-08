package tj.payment.wallet.ui.paycheck

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import tj.payment.core.Currency
import tj.payment.core.ErrorCode
import tj.payment.core.Money
import tj.payment.wallet.ui.KeyValueRow
import tj.payment.wallet.ui.PrimaryButton
import tj.payment.wallet.ui.ScreenHeader
import tj.payment.wallet.ui.appFieldColors
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust

@Composable
fun PayCheckScreen(
    viewModel: PayCheckViewModel,
    onClose: () -> Unit,
    onVerifyIdentity: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 20.dp)
            .imePadding(),
    ) {
        ScreenHeader(
            title = when (state.step) {
                PayStep.SCAN -> "Pay by QR"
                PayStep.PREVIEW -> "Confirm payment"
                PayStep.RESULT -> "Payment"
            },
            onBack = {
                when (state.step) {
                    PayStep.SCAN -> onClose()
                    PayStep.PREVIEW -> viewModel.backToScan()
                    PayStep.RESULT -> if (!state.submitting) onClose()
                }
            },
        )
        when (state.step) {
            PayStep.SCAN -> ScanStep(viewModel)
            PayStep.PREVIEW -> PreviewStep(viewModel)
            PayStep.RESULT -> ResultStep(viewModel, onClose, onVerifyIdentity)
        }
    }
}

@Composable
private fun ScanStep(viewModel: PayCheckViewModel) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val scanner = rememberLauncherForActivityResult(ScanContract()) { result ->
        result.contents?.let(viewModel::onScanned)
    }

    Column(Modifier.verticalScroll(rememberScrollState())) {
        Spacer(Modifier.height(16.dp))
        Text(
            "Point the camera at the merchant's QR code, or enter the check code shown on their screen.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(20.dp))
        PrimaryButton(
            text = "Scan QR code",
            onClick = {
                scanner.launch(
                    ScanOptions().apply {
                        setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                        setPrompt("Scan the merchant's payment code")
                        setBeepEnabled(false)
                        setOrientationLocked(false)
                    },
                )
            },
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(28.dp))
        Text(
            "Or enter the code",
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurface,
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = state.codeText,
            onValueChange = viewModel::onCodeChange,
            label = { Text("Check code") },
            singleLine = true,
            isError = state.scanError != null,
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Go),
            colors = appFieldColors(),
            modifier = Modifier.fillMaxWidth(),
        )
        state.scanError?.let {
            Spacer(Modifier.height(6.dp))
            Text(it, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
        }
        Spacer(Modifier.height(12.dp))
        PrimaryButton(
            text = "Look up",
            onClick = viewModel::lookUp,
            enabled = state.canLookUp,
            loading = state.lookingUp,
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

@Composable
private fun PreviewStep(viewModel: PayCheckViewModel) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val check = state.check ?: return
    val amount = check.money()
    val fee = state.feeMinor ?: 0L
    val currency = Currency.of(check.currency)

    Column(Modifier.verticalScroll(rememberScrollState())) {
        Spacer(Modifier.height(8.dp))
        Card(
            modifier = Modifier.fillMaxWidth(),
            shape = RoundedCornerShape(20.dp),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
            elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        ) {
            Column(Modifier.padding(20.dp)) {
                Text(
                    amount.format(),
                    fontSize = 34.sp,
                    fontWeight = FontWeight.SemiBold,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                Spacer(Modifier.height(12.dp))
                KeyValueRow("To", state.merchantLabel)
                if (check.merchantName == null) {
                    Text(
                        "This merchant has no verified name — pay only if you trust the code you scanned.",
                        style = MaterialTheme.typography.bodyMedium,
                        fontSize = 12.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                check.description?.let { KeyValueRow("For", it) }
                if (fee > 0) {
                    KeyValueRow("Fee (paid by merchant)", Money.ofMinor(fee, currency).format())
                }
                state.fromWallet?.let { w ->
                    KeyValueRow(
                        "From your wallet",
                        w.money().format(),
                        valueColor = if (state.insufficient) NegativeRed else PositiveGreen,
                    )
                }
            }
        }
        Spacer(Modifier.height(12.dp))
        if (state.insufficient) {
            Text(
                "Not enough balance for this payment.",
                color = NegativeRed,
                style = MaterialTheme.typography.bodyMedium,
            )
        } else if (state.fromWallet == null) {
            Text(
                "You have no ${check.currency} wallet to pay from.",
                color = NegativeRed,
                style = MaterialTheme.typography.bodyMedium,
            )
        } else {
            Text(
                "You'll confirm with your fingerprint or screen lock. Once paid, a payment can't be pulled back.",
                style = MaterialTheme.typography.bodyMedium,
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        state.gateMessage?.let {
            Spacer(Modifier.height(8.dp))
            Text(it, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
        }
        Spacer(Modifier.height(20.dp))
        // The device approval (fingerprint/face or screen lock, bound to a
        // Keystore key) is asked by the payment submitter itself — no screen
        // can start a payment without it.
        PrimaryButton(
            text = "Pay ${amount.format()}",
            onClick = viewModel::pay,
            enabled = state.canPay,
            loading = state.submitting,
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

@Composable
private fun ResultStep(viewModel: PayCheckViewModel, onClose: () -> Unit, onVerifyIdentity: () -> Unit) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    Column(
        modifier = Modifier.fillMaxSize(),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Spacer(Modifier.height(48.dp))
        when (val outcome = state.outcome) {
            null -> {
                Spacer(Modifier.height(24.dp))
                CircularProgressIndicator(color = Rust)
                Spacer(Modifier.height(16.dp))
                Text(
                    "Finishing your payment…",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            is PayOutcome.Success -> {
                ResultGlyph(success = true)
                Spacer(Modifier.height(20.dp))
                Text("Paid", style = MaterialTheme.typography.headlineMedium, color = MaterialTheme.colorScheme.onBackground)
                Spacer(Modifier.height(6.dp))
                Text(
                    "${state.confirmedAmount?.format() ?: ""} to ${state.confirmedLabel}" +
                        if (outcome.alreadyPosted) " (was already paid)" else "",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton("Done", onClick = onClose, modifier = Modifier.fillMaxWidth())
            }

            is PayOutcome.Rejected -> {
                ResultGlyph(success = false)
                Spacer(Modifier.height(20.dp))
                Text("Not paid", style = MaterialTheme.typography.headlineMedium, color = MaterialTheme.colorScheme.onBackground)
                Spacer(Modifier.height(6.dp))
                Text(outcome.message, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.height(32.dp))
                if (outcome.code == ErrorCode.KYC_REQUIRED) {
                    PrimaryButton("Verify now", onClick = onVerifyIdentity, modifier = Modifier.fillMaxWidth())
                    TextButton(onClick = viewModel::backToScan) {
                        Text("Back", color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                } else {
                    PrimaryButton("Scan another code", onClick = viewModel::backToScan, modifier = Modifier.fillMaxWidth())
                    TextButton(onClick = onClose) {
                        Text("Close", color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }

            is PayOutcome.Unsettled -> {
                ResultGlyph(success = false, warning = true)
                Spacer(Modifier.height(20.dp))
                Text("Not confirmed yet", style = MaterialTheme.typography.headlineMedium, color = MaterialTheme.colorScheme.onBackground)
                Spacer(Modifier.height(6.dp))
                Text(
                    (if (outcome.offline) "You appear to be offline. " else "The server didn't answer. ") +
                        "Your payment may still go through — retrying is always safe, you can never be charged twice.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton("Try again", onClick = viewModel::retry, loading = state.submitting, modifier = Modifier.fillMaxWidth())
                TextButton(onClick = onClose) {
                    Text("Later", color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
    }
}

@Composable
private fun ResultGlyph(success: Boolean, warning: Boolean = false) {
    val color = when {
        success -> PositiveGreen
        warning -> Rust
        else -> NegativeRed
    }
    Box(
        modifier = Modifier
            .size(84.dp)
            .background(color.copy(alpha = 0.14f), CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Canvas(Modifier.size(38.dp)) {
            val s = size.minDimension
            val stroke = s * 0.11f
            when {
                success -> {
                    drawLine(color, Offset(s * 0.16f, s * 0.55f), Offset(s * 0.42f, s * 0.78f), stroke, StrokeCap.Round)
                    drawLine(color, Offset(s * 0.42f, s * 0.78f), Offset(s * 0.84f, s * 0.26f), stroke, StrokeCap.Round)
                }
                warning -> {
                    drawLine(color, Offset(s * 0.5f, s * 0.16f), Offset(s * 0.5f, s * 0.6f), stroke, StrokeCap.Round)
                    drawCircle(color, radius = stroke * 0.55f, center = Offset(s * 0.5f, s * 0.82f))
                }
                else -> {
                    drawLine(color, Offset(s * 0.24f, s * 0.24f), Offset(s * 0.76f, s * 0.76f), stroke, StrokeCap.Round)
                    drawLine(color, Offset(s * 0.76f, s * 0.24f), Offset(s * 0.24f, s * 0.76f), stroke, StrokeCap.Round)
                }
            }
        }
    }
}
