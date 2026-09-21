package tj.payment.wallet.ui.home

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.slideInVertically
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
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tj.payment.core.WalletDto
import tj.payment.wallet.ui.ActionButton
import tj.payment.wallet.ui.ErrorRetry
import tj.payment.wallet.ui.GlyphArrow
import tj.payment.wallet.ui.GlyphList
import tj.payment.wallet.ui.GlyphSwap
import tj.payment.wallet.ui.TransactionRow
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.Rust
import tj.payment.wallet.ui.theme.RustBright

@Composable
fun HomeScreen(
    viewModel: HomeViewModel,
    onSend: () -> Unit,
    onReceive: () -> Unit,
    onHistory: (walletId: String) -> Unit,
    onConvert: () -> Unit,
    onKyc: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    // Fresh balances whenever Home comes (back) on screen — first entry, return
    // from Send/KYC/FX, return from the background — rate-limited in the VM.
    LifecycleResumeEffect(Unit) {
        viewModel.refreshIfDue()
        onPauseOrDispose { }
    }

    LazyColumn(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 20.dp),
    ) {
        item(key = "header") {
            Spacer(Modifier.height(24.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = "Your money",
                    style = MaterialTheme.typography.headlineMedium,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                TextButton(onClick = viewModel::logout) {
                    Text("Sign out", color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            Spacer(Modifier.height(12.dp))
        }

        // Shown once after secure storage had to be rebuilt: a payment may have
        // been in flight and its record is gone — History is the truth.
        if (state.unreadableRecordNotice) {
            item(key = "unreadable-notice") {
                NoticeCard(
                    title = "Check your history before sending",
                    body = "An earlier payment record on this device was unreadable and had to be " +
                        "removed. If you were in the middle of a payment, check History to see " +
                        "whether it went through before sending it again.",
                    onDismiss = viewModel::dismissUnreadableRecordNotice,
                )
                Spacer(Modifier.height(16.dp))
            }
        }

        // Identity: nudge level-0 users toward verification, show review state.
        if (state.kycLevel == 0) {
            item(key = "kyc-banner") {
                KycBanner(pending = state.kycPending, onClick = onKyc)
                Spacer(Modifier.height(16.dp))
            }
        }

        when {
            state.loading && state.wallets.isEmpty() -> item(key = "loading") {
                Box(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(180.dp),
                    contentAlignment = Alignment.Center,
                ) { CircularProgressIndicator(color = Rust) }
            }

            state.error != null && state.wallets.isEmpty() -> item(key = "error") {
                ErrorRetry(state.error ?: "", onRetry = viewModel::refresh)
            }

            else -> {
                // A failed refresh over cached balances: say so, keep showing them.
                if (state.error != null) {
                    item(key = "stale-error") {
                        ErrorRetry(state.error ?: "", onRetry = viewModel::refresh)
                        Spacer(Modifier.height(12.dp))
                    }
                }

                itemsIndexed(state.wallets, key = { _, w -> w.id }) { index, wallet ->
                    val visible = remember {
                        MutableTransitionState(false).apply { targetState = true }
                    }
                    AnimatedVisibility(
                        visibleState = visible,
                        enter = fadeIn(tween(350, delayMillis = index * 60)) +
                            slideInVertically(tween(350, delayMillis = index * 60)) { it / 4 },
                    ) {
                        Column {
                            WalletCard(wallet, primary = index == 0)
                            Spacer(Modifier.height(14.dp))
                        }
                    }
                }

                item(key = "actions") {
                    Spacer(Modifier.height(10.dp))
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceEvenly,
                    ) {
                        ActionButton("Send", onClick = onSend) { GlyphArrow(up = true) }
                        ActionButton("Receive", onClick = onReceive) { GlyphArrow(up = false) }
                        ActionButton("History", onClick = {
                            state.primaryWallet?.let { onHistory(it.id) }
                        }) { GlyphList() }
                        ActionButton("Convert", onClick = onConvert) { GlyphSwap() }
                    }
                    Spacer(Modifier.height(24.dp))
                }

                if (state.recent.isNotEmpty()) {
                    item(key = "recent-header") {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(
                                "Recent",
                                style = MaterialTheme.typography.titleLarge,
                                color = MaterialTheme.colorScheme.onBackground,
                            )
                            TextButton(onClick = {
                                state.primaryWallet?.let { onHistory(it.id) }
                            }) { Text("See all", color = Rust) }
                        }
                    }
                    itemsIndexed(state.recent, key = { _, e -> e.entryId }) { _, entry ->
                        TransactionRow(entry)
                    }
                    item(key = "recent-bottom") { Spacer(Modifier.height(24.dp)) }
                }
            }
        }
    }
}

@Composable
private fun NoticeCard(title: String, body: String, onDismiss: () -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = NegativeRed.copy(alpha = 0.14f)),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
    ) {
        Column(Modifier.padding(16.dp)) {
            Text(
                text = title,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                text = body,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            TextButton(onClick = onDismiss, modifier = Modifier.align(Alignment.End)) {
                Text("Got it", color = Rust)
            }
        }
    }
}

@Composable
private fun KycBanner(pending: Boolean, onClick: () -> Unit) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(enabled = !pending, onClick = onClick),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(
            containerColor = if (pending) {
                MaterialTheme.colorScheme.surface
            } else {
                Rust.copy(alpha = 0.16f)
            },
        ),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
    ) {
        Column(Modifier.padding(16.dp)) {
            Text(
                text = if (pending) "Verification under review" else "Verify your identity",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                text = if (pending) {
                    "We're checking your document. This usually takes less than a day."
                } else {
                    "Sending money requires a one-time identity check. Tap to start."
                },
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun WalletCard(wallet: WalletDto, primary: Boolean) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(20.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
    ) {
        Row(Modifier.padding(20.dp), verticalAlignment = Alignment.CenterVertically) {
            // A rust accent rail on the primary wallet gives the screen a focal point.
            Box(
                modifier = Modifier
                    .size(width = 4.dp, height = 44.dp)
                    .clip(RoundedCornerShape(2.dp))
                    .then(
                        if (primary) {
                            Modifier.background(
                                Brush.verticalGradient(listOf(RustBright, Rust)),
                            )
                        } else {
                            Modifier.background(MaterialTheme.colorScheme.outline)
                        },
                    ),
            )
            Spacer(Modifier.size(16.dp))
            Column {
                Text(
                    text = wallet.currency,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(4.dp))
                Text(
                    text = wallet.money().formatAmount(),
                    fontSize = 30.sp,
                    fontWeight = FontWeight.SemiBold,
                    color = MaterialTheme.colorScheme.onSurface,
                )
            }
        }
    }
}
