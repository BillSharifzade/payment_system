package tj.payment.wallet.ui.send

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tj.payment.core.Currency
import tj.payment.core.Money
import tj.payment.wallet.ui.KeyValueRow
import tj.payment.wallet.ui.PrimaryButton
import tj.payment.wallet.ui.ScreenHeader
import tj.payment.wallet.ui.appFieldColors
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust

@Composable
fun SendScreen(
    viewModel: SendViewModel,
    onClose: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .imePadding()
            .padding(horizontal = 20.dp),
    ) {
        ScreenHeader(
            title = when (state.step) {
                SendStep.RECIPIENT -> "Send money"
                SendStep.AMOUNT -> "Amount"
                SendStep.CONFIRM -> "Confirm"
                SendStep.RESULT -> "Payment"
            },
            onBack = {
                when (state.step) {
                    SendStep.RECIPIENT, SendStep.RESULT -> onClose()
                    SendStep.AMOUNT -> viewModel.backTo(SendStep.RECIPIENT)
                    SendStep.CONFIRM -> viewModel.backTo(SendStep.AMOUNT)
                }
            },
        )

        AnimatedContent(
            targetState = state.step,
            transitionSpec = {
                (slideInHorizontally(tween(250)) { it / 6 } + fadeIn(tween(250)))
                    .togetherWith(slideOutHorizontally(tween(200)) { -it / 8 } + fadeOut(tween(200)))
            },
            label = "sendStep",
        ) { step ->
            when (step) {
                SendStep.RECIPIENT -> RecipientStep(viewModel)
                SendStep.AMOUNT -> AmountStep(viewModel)
                SendStep.CONFIRM -> ConfirmStep(viewModel)
                SendStep.RESULT -> ResultStep(viewModel, onClose)
            }
        }
    }
}

// --- Step 1: who ---

@Composable
private fun RecipientStep(viewModel: SendViewModel) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    Column {
        state.pendingResume?.let { pending ->
            Card(
                modifier = Modifier.fillMaxWidth(),
                shape = RoundedCornerShape(16.dp),
                colors = CardDefaults.cardColors(containerColor = Rust.copy(alpha = 0.16f)),
                elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
            ) {
                Column(Modifier.padding(16.dp)) {
                    Text(
                        "Unfinished payment",
                        style = MaterialTheme.typography.titleMedium,
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "${Money.ofMinor(pending.amountMinor, Currency.of(pending.currency)).format()} " +
                            "to ${pending.recipientLabel} didn't finish. " +
                            "It may or may not have gone through — finish it before sending anything new.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(10.dp))
                    Row {
                        PrimaryButton(
                            text = "Finish it",
                            onClick = viewModel::resumePending,
                            modifier = Modifier.weight(1f),
                        )
                        TextButton(
                            onClick = viewModel::discardPending,
                            modifier = Modifier.align(Alignment.CenterVertically),
                        ) {
                            Text("Discard", color = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                    }
                }
            }
            Spacer(Modifier.height(20.dp))
        }

        Text(
            "Who are you sending to?",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(12.dp))
        OutlinedTextField(
            value = state.phone,
            onValueChange = viewModel::onPhoneChange,
            label = { Text("Recipient's phone number") },
            singleLine = true,
            enabled = state.pendingResume == null,
            shape = RoundedCornerShape(14.dp),
            colors = appFieldColors(),
            keyboardOptions = KeyboardOptions(
                keyboardType = KeyboardType.Phone,
                imeAction = ImeAction.Done,
            ),
            modifier = Modifier.fillMaxWidth(),
        )
        state.recipientError?.let {
            Spacer(Modifier.height(10.dp))
            Text(it, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
        }
        Spacer(Modifier.height(20.dp))
        PrimaryButton(
            text = "Check number",
            onClick = viewModel::checkRecipient,
            enabled = state.canCheckPhone && state.pendingResume == null,
            loading = state.resolving,
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

// --- Step 2: how much ---

@Composable
private fun AmountStep(viewModel: SendViewModel) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val currency = state.fromWallet?.currency ?: "TJS"

    Column(Modifier.fillMaxSize()) {
        RecipientChip(
            label = state.recipientLabel,
            verified = state.recipient?.nameVerified == true,
        )

        Spacer(Modifier.weight(1f))

        // The typed amount, front and center.
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.Center,
            verticalAlignment = Alignment.Bottom,
        ) {
            Text(
                text = state.amountText.ifEmpty { "0" },
                fontSize = 54.sp,
                fontWeight = FontWeight.SemiBold,
                color = if (state.insufficient) NegativeRed else MaterialTheme.colorScheme.onBackground,
            )
            Spacer(Modifier.size(8.dp))
            Text(
                text = currency,
                style = MaterialTheme.typography.titleLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(bottom = 10.dp),
            )
        }
        Spacer(Modifier.height(8.dp))
        val hint = when {
            state.insufficient -> "Not enough balance — you have " +
                (state.fromWallet?.money()?.formatAmount() ?: "0") + " $currency"
            else -> {
                val fee = state.feeMinor
                val amount = state.amount
                if (fee != null && amount != null && fee > 0) {
                    val gets = Money.ofMinor(amount.minorUnits - fee, amount.currency)
                    "Fee ${Money.ofMinor(fee, amount.currency).formatAmount()} · " +
                        "they receive ${gets.formatAmount()} $currency"
                } else {
                    "Balance: ${state.fromWallet?.money()?.formatAmount() ?: "…"} $currency"
                }
            }
        }
        Text(
            text = hint,
            style = MaterialTheme.typography.bodyMedium,
            color = if (state.insufficient) NegativeRed else MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 8.dp),
            maxLines = 1,
        )

        Spacer(Modifier.weight(1f))

        AmountKeypad(
            onDigit = viewModel::keyDigit,
            onComma = viewModel::keyComma,
            onBackspace = viewModel::keyBackspace,
        )
        Spacer(Modifier.height(16.dp))
        PrimaryButton(
            text = "Continue",
            onClick = viewModel::toConfirm,
            enabled = state.canContinueAmount,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(16.dp))
    }
}

@Composable
private fun RecipientChip(label: String, verified: Boolean) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surface, RoundedCornerShape(14.dp))
            .padding(horizontal = 14.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                label,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
                maxLines = 1,
            )
            Text(
                if (verified) "Verified name" else "Name not verified",
                style = MaterialTheme.typography.bodyMedium,
                fontSize = 12.sp,
                color = if (verified) PositiveGreen else MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        if (verified) {
            Canvas(Modifier.size(18.dp)) {
                val s = size.minDimension
                drawCircle(PositiveGreen.copy(alpha = 0.2f))
                val stroke = s * 0.12f
                drawLine(PositiveGreen, Offset(s * 0.28f, s * 0.52f), Offset(s * 0.45f, s * 0.68f), stroke, StrokeCap.Round)
                drawLine(PositiveGreen, Offset(s * 0.45f, s * 0.68f), Offset(s * 0.74f, s * 0.34f), stroke, StrokeCap.Round)
            }
        }
    }
}

@Composable
private fun AmountKeypad(
    onDigit: (Char) -> Unit,
    onComma: () -> Unit,
    onBackspace: () -> Unit,
) {
    val rows = listOf("123", "456", "789")
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (row in rows) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                for (ch in row) {
                    KeypadKey(
                        modifier = Modifier.weight(1f),
                        onClick = { onDigit(ch) },
                    ) { KeyLabel(ch.toString()) }
                }
            }
        }
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            KeypadKey(modifier = Modifier.weight(1f), onClick = onComma) { KeyLabel(",") }
            KeypadKey(modifier = Modifier.weight(1f), onClick = { onDigit('0') }) { KeyLabel("0") }
            KeypadKey(modifier = Modifier.weight(1f), onClick = onBackspace) {
                // Backspace glyph: left-pointing chevron with a cross-bar feel.
                val color = MaterialTheme.colorScheme.onSurface
                Canvas(Modifier.size(20.dp)) {
                    val s = size.minDimension
                    val stroke = s * 0.11f
                    drawLine(color, Offset(s * 0.15f, s * 0.5f), Offset(s * 0.85f, s * 0.5f), stroke, StrokeCap.Round)
                    drawLine(color, Offset(s * 0.15f, s * 0.5f), Offset(s * 0.42f, s * 0.26f), stroke, StrokeCap.Round)
                    drawLine(color, Offset(s * 0.15f, s * 0.5f), Offset(s * 0.42f, s * 0.74f), stroke, StrokeCap.Round)
                }
            }
        }
    }
}

@Composable
private fun KeypadKey(
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
    content: @Composable () -> Unit,
) {
    Box(
        modifier = modifier
            .height(58.dp)
            .background(MaterialTheme.colorScheme.surface, RoundedCornerShape(14.dp))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) { content() }
}

@Composable
private fun KeyLabel(text: String) {
    Text(
        text,
        fontSize = 22.sp,
        fontWeight = FontWeight.Medium,
        color = MaterialTheme.colorScheme.onSurface,
    )
}

// --- Step 3: confirm ---

@Composable
private fun ConfirmStep(viewModel: SendViewModel) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val amount = state.amount
    val fee = state.feeMinor ?: 0L
    val currency = amount?.currency ?: Currency.TJS

    Column {
        Spacer(Modifier.height(8.dp))
        Card(
            modifier = Modifier.fillMaxWidth(),
            shape = RoundedCornerShape(20.dp),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
            elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        ) {
            Column(Modifier.padding(20.dp)) {
                KeyValueRow("To", state.recipientLabel)
                if (state.recipient?.nameVerified != true) {
                    Text(
                        "This account has no verified name — double-check the number.",
                        style = MaterialTheme.typography.bodyMedium,
                        fontSize = 12.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Spacer(Modifier.height(8.dp))
                KeyValueRow("You send", amount?.format() ?: "—")
                if (fee > 0 && amount != null) {
                    KeyValueRow("Fee", Money.ofMinor(fee, currency).format())
                    KeyValueRow(
                        "They receive",
                        Money.ofMinor(amount.minorUnits - fee, currency).format(),
                        valueColor = PositiveGreen,
                    )
                }
            }
        }
        Spacer(Modifier.height(12.dp))
        Text(
            "Once sent, a payment can't be pulled back. Make sure the recipient is right.",
            style = MaterialTheme.typography.bodyMedium,
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(20.dp))
        PrimaryButton(
            text = "Send ${amount?.format() ?: ""}",
            onClick = viewModel::confirmAndSend,
            loading = state.submitting,
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

// --- Step 4: outcome ---

@Composable
private fun ResultStep(viewModel: SendViewModel, onClose: () -> Unit) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    Column(
        modifier = Modifier.fillMaxSize(),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Spacer(Modifier.height(48.dp))
        when (val outcome = state.outcome) {
            null -> {
                // Still submitting (resume path lands here with a spinner).
                Spacer(Modifier.height(24.dp))
                androidx.compose.material3.CircularProgressIndicator(color = Rust)
                Spacer(Modifier.height(16.dp))
                Text(
                    "Finishing your payment…",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            is SendOutcome.Success -> {
                ResultGlyph(success = true)
                Spacer(Modifier.height(20.dp))
                Text(
                    "Sent",
                    style = MaterialTheme.typography.headlineMedium,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    "${state.confirmedAmount?.format() ?: ""} to ${state.confirmedLabel}" +
                        if (outcome.alreadyPosted) " (was already sent)" else "",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton("Done", onClick = onClose, modifier = Modifier.fillMaxWidth())
            }

            is SendOutcome.Rejected -> {
                ResultGlyph(success = false)
                Spacer(Modifier.height(20.dp))
                Text(
                    "Not sent",
                    style = MaterialTheme.typography.headlineMedium,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    outcome.message,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton(
                    "Back",
                    onClick = { viewModel.backTo(SendStep.AMOUNT) },
                    modifier = Modifier.fillMaxWidth(),
                )
            }

            is SendOutcome.Unsettled -> {
                ResultGlyph(success = false, warning = true)
                Spacer(Modifier.height(20.dp))
                Text(
                    "Not confirmed yet",
                    style = MaterialTheme.typography.headlineMedium,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    (if (outcome.offline) "You appear to be offline. " else "The server didn't answer. ") +
                        "Your payment may still go through — retrying is always safe, " +
                        "you can never be charged twice.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(32.dp))
                PrimaryButton(
                    "Try again",
                    onClick = viewModel::retry,
                    loading = state.submitting,
                    modifier = Modifier.fillMaxWidth(),
                )
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
