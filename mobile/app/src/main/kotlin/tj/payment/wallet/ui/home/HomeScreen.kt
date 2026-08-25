package tj.payment.wallet.ui.home

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.slideInVertically
import androidx.compose.foundation.background
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
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tj.payment.core.WalletDto
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.Rust
import tj.payment.wallet.ui.theme.RustBright

@Composable
fun HomeScreen(
    viewModel: HomeViewModel,
    onLoggedOut: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()

    if (state.loggedOut) {
        onLoggedOut()
        return
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 20.dp),
    ) {
        Spacer(Modifier.height(28.dp))
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "Your wallets",
                style = MaterialTheme.typography.headlineMedium,
                color = MaterialTheme.colorScheme.onBackground,
            )
            TextButton(onClick = viewModel::logout) {
                Text("Sign out", color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }

        Spacer(Modifier.height(16.dp))

        when {
            state.loading -> Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(180.dp),
                contentAlignment = Alignment.Center,
            ) {
                CircularProgressIndicator(color = Rust)
            }

            state.error != null -> Column {
                Text(
                    state.error ?: "",
                    color = NegativeRed,
                    style = MaterialTheme.typography.bodyMedium,
                )
                Spacer(Modifier.height(8.dp))
                TextButton(onClick = viewModel::refresh) {
                    Text("Retry", color = Rust)
                }
            }

            state.wallets.isEmpty() -> Text(
                "No wallets yet.",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                style = MaterialTheme.typography.bodyMedium,
            )

            else -> LazyColumn(verticalArrangement = Arrangement.spacedBy(14.dp)) {
                itemsIndexed(state.wallets, key = { _, w -> w.id }) { index, wallet ->
                    val visible = remember {
                        MutableTransitionState(false).apply { targetState = true }
                    }
                    AnimatedVisibility(
                        visibleState = visible,
                        enter = fadeIn(tween(350, delayMillis = index * 60)) +
                            slideInVertically(tween(350, delayMillis = index * 60)) { it / 4 },
                    ) {
                        WalletCard(wallet, primary = index == 0)
                    }
                }
            }
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
