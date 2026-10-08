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
import androidx.compose.ui.res.stringResource
import tj.payment.wallet.R

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
                PayStep.SCAN -> stringResource(R.string.pay_title)
                PayStep.PREVIEW -> stringResource(R.string.auth_prompt_title)
                PayStep.RESULT -> stringResource(R.string.payment_title)
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
    val scanPrompt = stringResource(R.string.pay_scan_prompt)

    Column(Modifier.verticalScroll(rememberScrollState())) {
        Spacer(Modifier.height(16.dp))
        Text(
            stringResource(R.string.pay_scan_body),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(20.dp))
        PrimaryButton(
            text = stringResource(R.string.pay_scan_action),
            onClick = {
                scanner.launch(
                    ScanOptions().apply {
                        setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                        setPrompt(scanPrompt)
                        setBeepEnabled(false)
                        setOrientationLocked(false)
                    },
                )
            },
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(28.dp))
        Text(
            stringResource(R.string.pay_or_enter_code),
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurface,
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = state.codeText,
            onValueChange = viewModel::onCodeChange,
            label = { Text(stringResource(R.string.pay_check_code)) },
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
            text = stringResource(R.string.pay_look_up),
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
                KeyValueRow(stringResource(R.string.label_to), state.merchantLabel)
                if (check.merchantName == null) {
                    Text(
                        stringResource(R.string.pay_unverified_warning),
                        style = MaterialTheme.typography.bodyMedium,
                        fontSize = 12.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                check.description?.let { KeyValueRow(stringResource(R.string.pay_for), it) }
                if (fee > 0) {
                    KeyValueRow(stringResource(R.string.pay_fee_merchant), Money.ofMinor(fee, currency).format())
                }
                state.fromWallet?.let { w ->
                    KeyValueRow(
                        stringResource(R.string.pay_from_wallet),
                        w.money().format(),
                        valueColor = if (state.insufficient) NegativeRed else PositiveGreen,
                    )
                }
            }
        }
        Spacer(Modifier.height(12.dp))
        if (state.insufficient) {
            Text(
                stringResource(R.string.pay_insufficient),
                color = NegativeRed,
                style = MaterialTheme.typography.bodyMedium,
            )
        } else if (state.fromWallet == null) {
            Text(
                stringResource(R.string.pay_no_wallet, check.currency),
                color = NegativeRed,
                style = MaterialTheme.typography.bodyMedium,
            )
        } else {
            Text(
                stringResource(R.string.pay_irreversible),
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
            text = stringResource(R.string.pay_action, amount.format()),
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
                    stringResource(R.string.payment_finishing),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            is PayOutcome.Success -> {
                ResultGlyph(success = true)
                Spacer(Modifier.height(20.dp))
                Text(stringResource(R.string.pay_result_paid), style = MaterialTheme.typography.headlineMedium, color = MaterialTheme.colorScheme.onBackground)
                Spacer(Modifier.height(6.dp))
                Text(
                    stringResource(
                        if (outcome.alreadyPosted) R.string.pay_result_detail_already else R.string.send_result_detail,
                        state.confirmedAmount?.format() ?: "",
                        state.confirmedLabel,
                    ),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton(stringResource(R.string.action_done), onClick = onClose, modifier = Modifier.fillMaxWidth())
            }

            is PayOutcome.Rejected -> {
                ResultGlyph(success = false)
                Spacer(Modifier.height(20.dp))
                Text(stringResource(R.string.pay_result_not_paid), style = MaterialTheme.typography.headlineMedium, color = MaterialTheme.colorScheme.onBackground)
                Spacer(Modifier.height(6.dp))
                Text(outcome.message, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.height(32.dp))
                if (outcome.code == ErrorCode.KYC_REQUIRED) {
                    PrimaryButton(stringResource(R.string.action_verify_now), onClick = onVerifyIdentity, modifier = Modifier.fillMaxWidth())
                    TextButton(onClick = viewModel::backToScan) {
                        Text(stringResource(R.string.action_back), color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                } else {
                    PrimaryButton(stringResource(R.string.pay_scan_another), onClick = viewModel::backToScan, modifier = Modifier.fillMaxWidth())
                    TextButton(onClick = onClose) {
                        Text(stringResource(R.string.action_close), color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }

            is PayOutcome.Unsettled -> {
                ResultGlyph(success = false, warning = true)
                Spacer(Modifier.height(20.dp))
                Text(stringResource(R.string.payment_not_confirmed), style = MaterialTheme.typography.headlineMedium, color = MaterialTheme.colorScheme.onBackground)
                Spacer(Modifier.height(6.dp))
                Text(
                    stringResource(if (outcome.offline) R.string.payment_unsettled_offline else R.string.payment_unsettled_no_answer),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton(stringResource(R.string.action_try_again), onClick = viewModel::retry, loading = state.submitting, modifier = Modifier.fillMaxWidth())
                TextButton(onClick = onClose) {
                    Text(stringResource(R.string.action_later), color = MaterialTheme.colorScheme.onSurfaceVariant)
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
