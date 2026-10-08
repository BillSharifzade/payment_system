package tj.payment.wallet.ui.request

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import com.google.zxing.qrcode.decoder.ErrorCorrectionLevel
import kotlinx.coroutines.delay
import tj.payment.core.CheckDto
import tj.payment.wallet.ui.KeyValueRow
import tj.payment.wallet.ui.PrimaryButton
import tj.payment.wallet.ui.ScreenHeader
import tj.payment.wallet.ui.appFieldColors
import tj.payment.wallet.ui.paycheck.CheckCode
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust
import androidx.compose.ui.res.stringResource
import tj.payment.wallet.R

@Composable
fun RequestScreen(viewModel: RequestViewModel, onBack: () -> Unit) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 20.dp)
            .imePadding()
            .verticalScroll(rememberScrollState()),
    ) {
        ScreenHeader(title = stringResource(R.string.request_title), onBack = onBack)
        val check = state.check
        if (check == null) AmountForm(viewModel) else CheckView(check, state, viewModel)
    }
}

@Composable
private fun AmountForm(viewModel: RequestViewModel) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    Spacer(Modifier.height(16.dp))
    Text(
        stringResource(R.string.request_body),
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    Spacer(Modifier.height(20.dp))
    OutlinedTextField(
        value = state.amountText,
        onValueChange = viewModel::onAmountChange,
        label = { Text(stringResource(R.string.request_amount_label, state.wallet?.currency ?: "TJS")) },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Decimal),
        colors = appFieldColors(),
        modifier = Modifier.fillMaxWidth(),
    )
    Spacer(Modifier.height(10.dp))
    OutlinedTextField(
        value = state.description,
        onValueChange = viewModel::onDescriptionChange,
        label = { Text(stringResource(R.string.request_description_label)) },
        singleLine = true,
        colors = appFieldColors(),
        modifier = Modifier.fillMaxWidth(),
    )
    state.error?.let {
        Spacer(Modifier.height(8.dp))
        Text(it, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
    }
    Spacer(Modifier.height(20.dp))
    PrimaryButton(
        text = state.amount?.let { stringResource(R.string.request_action_amount, it.format()) } ?: stringResource(R.string.request_action),
        onClick = viewModel::create,
        enabled = state.canCreate,
        loading = state.creating,
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
private fun CheckView(check: CheckDto, state: RequestUiState, viewModel: RequestViewModel) {
    val clipboard = LocalClipboardManager.current
    var copied by remember { mutableStateOf(false) }
    LaunchedEffect(copied) {
        if (copied) {
            delay(1800)
            copied = false
        }
    }
    val paid = check.status == "paid"

    Spacer(Modifier.height(16.dp))
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surface, RoundedCornerShape(20.dp))
            .padding(20.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(
            check.money().format(),
            fontSize = 34.sp,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.onSurface,
        )
        check.description?.let {
            Text(it, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.height(16.dp))
        when {
            paid -> {
                Text(stringResource(R.string.request_paid), color = PositiveGreen, fontSize = 28.sp, fontWeight = FontWeight.Bold)
                check.payerName?.let {
                    Text(stringResource(R.string.request_paid_by, it), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            !check.isOpen -> Text(
                when (check.status) {
                    "cancelled" -> stringResource(R.string.request_status_cancelled)
                    "expired" -> stringResource(R.string.request_status_expired)
                    else -> check.status.replaceFirstChar { it.uppercase() }
                },
                color = NegativeRed,
                fontSize = 24.sp,
                fontWeight = FontWeight.Bold,
            )
            else -> {
                QrCode(
                    content = CheckCode.encode(check.id),
                    modifier = Modifier
                        .fillMaxWidth(0.72f)
                        .aspectRatio(1f),
                )
                Spacer(Modifier.height(12.dp))
                Text(
                    stringResource(
                        R.string.request_waiting,
                        "${state.secondsLeft / 60}:${"%02d".format(state.secondsLeft % 60)}",
                    ),
                    style = MaterialTheme.typography.bodyMedium,
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        Spacer(Modifier.height(14.dp))
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceVariant, RoundedCornerShape(14.dp))
                .clickable {
                    clipboard.setText(AnnotatedString(check.id))
                    copied = true
                }
                .padding(horizontal = 14.dp, vertical = 10.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                check.id,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Text(
                stringResource(if (copied) R.string.receive_copied else R.string.request_code_tap_to_copy),
                fontSize = 11.sp,
                color = if (copied) PositiveGreen else Rust,
            )
        }
    }
    Spacer(Modifier.height(16.dp))
    if (paid) {
        KeyValueRow(stringResource(R.string.request_transaction), check.transactionId?.take(8)?.let { "$it…" } ?: "—")
    }
    Spacer(Modifier.height(8.dp))
    if (check.isOpen) {
        TextButton(onClick = viewModel::cancel, enabled = !state.cancelling, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.request_cancel), color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
    PrimaryButton(
        text = stringResource(if (check.isOpen) R.string.request_new else R.string.action_done),
        onClick = viewModel::newRequest,
        modifier = Modifier.fillMaxWidth(),
    )
    Spacer(Modifier.height(24.dp))
}

/**
 * The check's QR, drawn straight from the ZXing bit matrix: no Bitmap, no
 * allocation per frame, crisp at any size. Quiet zone included by the writer.
 */
@Composable
private fun QrCode(content: String, modifier: Modifier = Modifier) {
    val matrix = remember(content) {
        QRCodeWriter().encode(
            content,
            BarcodeFormat.QR_CODE,
            0,
            0,
            mapOf(EncodeHintType.ERROR_CORRECTION to ErrorCorrectionLevel.M, EncodeHintType.MARGIN to 2),
        )
    }
    Box(
        modifier = modifier.background(Color.White, RoundedCornerShape(12.dp)),
        contentAlignment = Alignment.Center,
    ) {
        Canvas(Modifier.fillMaxSize().padding(8.dp)) {
            val cell = size.minDimension / matrix.width
            val offsetX = (size.width - cell * matrix.width) / 2f
            val offsetY = (size.height - cell * matrix.height) / 2f
            for (y in 0 until matrix.height) {
                for (x in 0 until matrix.width) {
                    if (matrix.get(x, y)) {
                        drawRect(
                            color = Color.Black,
                            topLeft = Offset(offsetX + x * cell, offsetY + y * cell),
                            size = Size(cell + 0.5f, cell + 0.5f),
                        )
                    }
                }
            }
        }
    }
}
