package tj.payment.wallet.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import tj.payment.core.Currency
import tj.payment.core.Money
import tj.payment.core.PaymentKind
import tj.payment.core.PendingPayment
import tj.payment.core.PendingPaymentResolver
import tj.payment.core.PendingPaymentResolver.Notice
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust

/** "50,00 TJS to +992…" / "Exchange of 50,00 TJS to USD" — what the user confirmed. */
fun PendingPayment.describe(): String {
    val amount = Money.ofMinor(amountMinor, Currency.of(currency)).format()
    return when (kind) {
        PaymentKind.FX -> "Exchange of $amount to $recipientLabel"
        PaymentKind.TRANSFER, PaymentKind.CHECK -> "$amount to $recipientLabel"
    }
}

/**
 * The current user's unsettled payment, with Finish (same-key retry) and
 * Discard (voids the key first), plus the outcome of the last resolution.
 * Renders nothing when there is nothing to say. Shared by Home and FX.
 */
@Composable
fun PendingPaymentCard(
    state: PendingPaymentResolver.State,
    onFinish: () -> Unit,
    onRequestDiscard: () -> Unit,
    onConfirmDiscard: () -> Unit,
    onCancelDiscard: () -> Unit,
    onDismissNotice: () -> Unit,
    modifier: Modifier = Modifier,
) {
    if (state.confirmDiscard) {
        AlertDialog(
            onDismissRequest = onCancelDiscard,
            containerColor = MaterialTheme.colorScheme.surface,
            titleContentColor = MaterialTheme.colorScheme.onSurface,
            textContentColor = MaterialTheme.colorScheme.onSurfaceVariant,
            title = { Text("Discard this payment?") },
            text = {
                Text(
                    "We'll cancel it with our server first. If it already went through, it will be " +
                        "shown as sent instead. Otherwise it is cancelled for good — it can never go " +
                        "through later.",
                )
            },
            confirmButton = { TextButton(onClick = onConfirmDiscard) { Text("Discard", color = NegativeRed) } },
            dismissButton = {
                TextButton(onClick = onCancelDiscard) {
                    Text("Keep it", color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
        )
    }

    val pending = state.pending
    val notice = state.notice
    if (pending == null && notice == null) return

    Column(modifier) {
        notice?.let { NoticeLine(it, onDismissNotice) }
        if (pending != null) {
            if (notice != null) Spacer(Modifier.height(8.dp))
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
                        "${pending.describe()} didn't finish. It may or may not have gone through — " +
                            "finishing it is always safe, you can never be charged twice.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(10.dp))
                    val busy = state.busy
                    Row {
                        PrimaryButton(
                            text = if (busy == PendingPaymentResolver.Busy.CHECKING) "Checking…" else "Finish it",
                            onClick = onFinish,
                            enabled = busy == null,
                            loading = busy == PendingPaymentResolver.Busy.RETRYING,
                            modifier = Modifier.weight(1f),
                        )
                        TextButton(
                            onClick = onRequestDiscard,
                            enabled = busy == null,
                            modifier = Modifier.align(Alignment.CenterVertically),
                        ) {
                            Text(
                                if (busy == PendingPaymentResolver.Busy.DISCARDING) "Cancelling…" else "Discard",
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun NoticeLine(notice: Notice, onDismiss: () -> Unit) {
    val (text, color) = when (notice) {
        is Notice.Sent -> "${notice.payment.describe()} went through." to PositiveGreen
        is Notice.Cancelled -> "${notice.payment.describe()} was cancelled. Nothing was sent." to MaterialTheme.colorScheme.onSurface
        is Notice.Refused -> "${notice.payment.describe()} wasn't sent: ${notice.code.userMessage()}" to NegativeRed
        is Notice.NoAnswer ->
            (if (notice.offline) "Still offline — " else "Still no answer from the server — ") +
                "the payment is kept. Try again in a moment." to Rust
        is Notice.DiscardFailed ->
            (if (notice.offline) "Can't cancel it while offline" else "Couldn't cancel it just now") +
                " — it's kept until we can. Try again in a moment." to Rust
        Notice.StorageProblem -> STORAGE_MESSAGE to NegativeRed
    }
    Card(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
    ) {
        Row(Modifier.padding(start = 16.dp, top = 8.dp, bottom = 8.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(text, color = color, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
            TextButton(onClick = onDismiss) { Text("OK", color = Rust) }
        }
    }
}
