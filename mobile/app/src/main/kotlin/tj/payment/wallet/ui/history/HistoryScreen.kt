package tj.payment.wallet.ui.history

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tj.payment.wallet.ui.ErrorRetry
import tj.payment.wallet.ui.ScreenHeader
import tj.payment.wallet.ui.TransactionRow
import tj.payment.wallet.ui.dayHeading
import tj.payment.wallet.ui.localDate
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.Rust
import androidx.compose.ui.res.stringResource
import tj.payment.wallet.R

@Composable
fun HistoryScreen(
    viewModel: HistoryViewModel,
    onBack: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val listState = rememberLazyListState()

    // Infinite scroll: ask for the next page when the tail comes into view.
    val nearEnd by remember {
        derivedStateOf {
            val info = listState.layoutInfo
            val last = info.visibleItemsInfo.lastOrNull()?.index ?: 0
            last >= info.totalItemsCount - 4
        }
    }
    LaunchedEffect(nearEnd, state.entries.size) {
        if (nearEnd) viewModel.loadMore()
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 20.dp),
    ) {
        ScreenHeader(title = stringResource(R.string.history_title), onBack = onBack)

        when {
            state.loading -> Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(200.dp),
                contentAlignment = Alignment.Center,
            ) { CircularProgressIndicator(color = Rust) }

            state.error != null -> ErrorRetry(state.error ?: "", onRetry = viewModel::refresh)

            state.entries.isEmpty() -> Column {
                Spacer(Modifier.height(40.dp))
                Text(
                    stringResource(R.string.history_empty_title),
                    style = MaterialTheme.typography.titleLarge,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    stringResource(R.string.history_empty_body),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            else -> LazyColumn(state = listState, modifier = Modifier.fillMaxSize()) {
                itemsIndexed(state.entries, key = { _, e -> e.entryId }) { index, entry ->
                    val date = entry.localDate()
                    val isNewDay = index == 0 ||
                        state.entries[index - 1].localDate() != date
                    if (isNewDay) {
                        Text(
                            text = dayHeading(date),
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(top = 18.dp, bottom = 4.dp),
                        )
                    }
                    TransactionRow(entry)
                }
                // An older page failed to load: say so where the list stopped,
                // with a retry, instead of an infinite scroll that silently ends.
                state.loadMoreError?.let { message ->
                    item(key = "load-more-error") {
                        Column(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(vertical = 12.dp),
                            horizontalAlignment = Alignment.CenterHorizontally,
                        ) {
                            Text(
                                stringResource(R.string.history_load_more_failed, message),
                                color = NegativeRed,
                                style = MaterialTheme.typography.bodyMedium,
                            )
                            TextButton(onClick = viewModel::retryLoadMore) { Text(stringResource(R.string.action_retry), color = Rust) }
                        }
                    }
                }
                if (state.loadingMore) {
                    item(key = "loading-more") {
                        Box(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(vertical = 16.dp),
                            contentAlignment = Alignment.Center,
                        ) {
                            CircularProgressIndicator(
                                color = Rust,
                                modifier = Modifier.height(24.dp),
                                strokeWidth = 2.dp,
                            )
                        }
                    }
                }
                item(key = "bottom-space") { Spacer(Modifier.height(24.dp)) }
            }
        }
    }
}
