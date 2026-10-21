package tj.payment.wallet.ui.fx

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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tj.payment.core.Currency
import tj.payment.core.Money
import tj.payment.wallet.ui.ErrorRetry
import tj.payment.wallet.ui.GlyphSwap
import tj.payment.wallet.ui.KeyValueRow
import tj.payment.wallet.ui.PendingPaymentCard
import tj.payment.wallet.ui.PrimaryButton
import tj.payment.wallet.ui.ScreenHeader
import tj.payment.wallet.ui.appFieldColors
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust
import androidx.compose.ui.res.stringResource
import tj.payment.wallet.R

@Composable
fun FxScreen(
    viewModel: FxViewModel,
    onBack: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val pending by viewModel.pending.state.collectAsStateWithLifecycle()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .imePadding()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp),
    ) {
        ScreenHeader(title = stringResource(R.string.fx_title), onBack = onBack)

        // An unsettled payment of this user (e.g. a conversion whose answer was
        // lost, even in an earlier app run) must be finished or discarded first.
        PendingPaymentCard(
            state = pending,
            onFinish = viewModel.pending::finish,
            onRequestDiscard = viewModel.pending::requestDiscard,
            onConfirmDiscard = viewModel.pending::confirmDiscard,
            onCancelDiscard = viewModel.pending::cancelDiscard,
            onDismissNotice = viewModel.pending::dismissNotice,
            modifier = Modifier.padding(bottom = 16.dp),
        )

        when {
            state.loading -> Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(160.dp),
                contentAlignment = Alignment.Center,
            ) { CircularProgressIndicator(color = Rust) }

            state.error != null -> ErrorRetry(state.error ?: "") { viewModel.refresh() }

            state.needsSecondWallet -> Column {
                Text(
                    stringResource(R.string.fx_need_second_title),
                    style = MaterialTheme.typography.titleLarge,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    stringResource(R.string.fx_need_second_body),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                state.actionError?.let {
                    Spacer(Modifier.height(10.dp))
                    Text(it, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
                }
                Spacer(Modifier.height(20.dp))
                PrimaryButton(
                    text = stringResource(R.string.fx_open_usd),
                    onClick = viewModel::openUsdWallet,
                    loading = state.openingWallet,
                    modifier = Modifier.fillMaxWidth(),
                )
            }

            else -> {
                // From / swap / to.
                WalletLine(
                    label = stringResource(R.string.fx_from),
                    currency = state.from?.currency ?: "—",
                    balance = state.from?.displayAmount(),
                )
                Box(
                    modifier = Modifier
                        .padding(vertical = 8.dp)
                        .size(40.dp)
                        .background(MaterialTheme.colorScheme.surfaceVariant, CircleShape)
                        .clickable(onClick = viewModel::swap)
                        .align(Alignment.CenterHorizontally),
                    contentAlignment = Alignment.Center,
                ) { GlyphSwap() }
                WalletLine(
                    label = stringResource(R.string.label_to),
                    currency = state.to?.currency ?: "—",
                    balance = state.to?.displayAmount(),
                )

                Spacer(Modifier.height(20.dp))
                OutlinedTextField(
                    value = state.amountText,
                    onValueChange = viewModel::onAmountChange,
                    label = { Text(stringResource(R.string.fx_amount_in, state.from?.currency ?: "")) },
                    singleLine = true,
                    isError = state.insufficient,
                    shape = RoundedCornerShape(14.dp),
                    colors = appFieldColors(),
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Decimal),
                    modifier = Modifier.fillMaxWidth(),
                )

                Spacer(Modifier.height(14.dp))
                val rate = state.rate
                when {
                    state.insufficient -> Text(
                        stringResource(R.string.fx_insufficient),
                        color = NegativeRed,
                        style = MaterialTheme.typography.bodyMedium,
                    )

                    rate == null -> Text(
                        stringResource(R.string.fx_no_rate),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodyMedium,
                    )

                    else -> Column {
                        val one = Money.ofMinor(
                            rate.convert(100) ?: 0,
                            Currency.of(rate.quote),
                        )
                        KeyValueRow(stringResource(R.string.fx_rate), "1 ${rate.base} = ${one.formatAmount()} ${rate.quote}")
                        state.quoteMinor?.let { q ->
                            KeyValueRow(
                                stringResource(R.string.fx_you_get),
                                Money.ofMinor(q, Currency.of(rate.quote)).format(),
                                valueColor = PositiveGreen,
                            )
                        }
                    }
                }

                state.actionError?.let {
                    Spacer(Modifier.height(10.dp))
                    // An unknown outcome is a caution (check balances, same-key
                    // retry is safe), not a refusal — rust, not red.
                    Text(
                        it,
                        color = if (state.outcomeUnknown) Rust else NegativeRed,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                }

                state.exchanged?.let { debited ->
                    Spacer(Modifier.height(14.dp))
                    Text(
                        stringResource(R.string.fx_exchanged_amount, debited.format()),
                        style = MaterialTheme.typography.titleMedium,
                        color = PositiveGreen,
                    )
                }

                state.result?.let { r ->
                    Spacer(Modifier.height(14.dp))
                    Column(
                        modifier = Modifier
                            .fillMaxWidth()
                            .background(PositiveGreen.copy(alpha = 0.12f), RoundedCornerShape(14.dp))
                            .padding(14.dp),
                    ) {
                        Text(
                            stringResource(R.string.fx_exchanged),
                            style = MaterialTheme.typography.titleMedium,
                            color = PositiveGreen,
                        )
                        Spacer(Modifier.height(4.dp))
                        Text(
                            "${Money.ofMinor(r.debitedMinor, Currency.of(r.fromCurrency)).format()} → " +
                                Money.ofMinor(r.creditedMinor, Currency.of(r.toCurrency)).format(),
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurface,
                        )
                    }
                }

                Spacer(Modifier.height(24.dp))
                PrimaryButton(
                    text = stringResource(R.string.fx_action),
                    onClick = viewModel::convert,
                    // Confirmed with the fingerprint / screen lock by the submitter.
                    enabled = state.canConvert && pending.pending == null && pending.busy == null,
                    loading = state.converting,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(24.dp))
            }
        }
    }
}

@Composable
private fun WalletLine(label: String, currency: String, balance: String?) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surface, RoundedCornerShape(14.dp))
            .padding(horizontal = 16.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Column {
            Text(
                label,
                style = MaterialTheme.typography.bodyMedium,
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(
                currency,
                style = MaterialTheme.typography.titleLarge,
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
        balance?.let {
            Text(
                it,
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}
