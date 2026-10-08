package tj.payment.wallet.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import tj.payment.core.StatementEntryDto
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.R

/** What a statement row calls the movement, from this wallet's point of view. */
fun StatementEntryDto.title(): String = when (kind) {
    "transfer" -> counterpartyName
        ?: counterpartyPhone?.let { "+$it" }
        ?: Copy.text(if (isCredit) R.string.tx_received else R.string.tx_sent)
    "deposit" -> Copy.text(R.string.tx_title_topup)
    "withdrawal" -> Copy.text(R.string.tx_withdrawal)
    "fx" -> Copy.text(R.string.tx_title_fx)
    "fee" -> Copy.text(R.string.tx_title_fee)
    else -> Copy.text(if (isCredit) R.string.tx_received else R.string.tx_sent)
}

fun StatementEntryDto.subtitle(): String {
    val what = Copy.text(
        when (kind) {
            "transfer" -> if (isCredit) R.string.tx_received else R.string.tx_sent
            "deposit" -> R.string.tx_deposit
            "withdrawal" -> R.string.tx_withdrawal
            "fx" -> if (isCredit) R.string.tx_bought else R.string.tx_sold
            "fee" -> R.string.tx_service_fee
            else -> R.string.tx_other
        },
    )
    return "$what · ${timeOfDay()}"
}

private val TIME = DateTimeFormatter.ofPattern("HH:mm")
private val DAY = DateTimeFormatter.ofPattern("d MMMM")
private val DAY_YEAR = DateTimeFormatter.ofPattern("d MMMM yyyy")

fun StatementEntryDto.localDate(): LocalDate =
    Instant.ofEpochMilli(createdAtMs).atZone(ZoneId.systemDefault()).toLocalDate()

fun StatementEntryDto.timeOfDay(): String =
    TIME.format(Instant.ofEpochMilli(createdAtMs).atZone(ZoneId.systemDefault()))

/** "Today", "Yesterday", "12 August" (+ year when not this year). */
fun dayHeading(date: LocalDate, today: LocalDate = LocalDate.now()): String = when (date) {
    today -> Copy.text(R.string.day_today)
    today.minusDays(1) -> Copy.text(R.string.day_yesterday)
    else -> if (date.year == today.year) DAY.format(date) else DAY_YEAR.format(date)
}

/** One movement: direction badge, who/what, signed colored amount. */
@Composable
fun TransactionRow(entry: StatementEntryDto, modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DirectionBadge(kind = entry.kind, isCredit = entry.isCredit)
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Text(
                entry.title(),
                style = MaterialTheme.typography.titleMedium,
                fontSize = 15.sp,
                color = MaterialTheme.colorScheme.onSurface,
                maxLines = 1,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                entry.subtitle(),
                style = MaterialTheme.typography.bodyMedium,
                fontSize = 13.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
            )
        }
        Spacer(Modifier.width(10.dp))
        Text(
            text = (if (entry.isCredit) "+" else "−") + entry.money().formatAmount() +
                " " + entry.currency,
            style = MaterialTheme.typography.titleMedium,
            fontSize = 15.sp,
            fontWeight = FontWeight.SemiBold,
            color = if (entry.isCredit) PositiveGreen else MaterialTheme.colorScheme.onSurface,
        )
    }
}
