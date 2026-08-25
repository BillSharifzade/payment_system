package tj.payment.wallet.ui.kyc

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import tj.payment.wallet.ui.ErrorRetry
import tj.payment.wallet.ui.PrimaryButton
import tj.payment.wallet.ui.ScreenHeader
import tj.payment.wallet.ui.appFieldColors
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust

@Composable
fun KycScreen(
    viewModel: KycViewModel,
    onBack: () -> Unit,
) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    // While a submission is under review, quietly re-check so approval shows up
    // without the user hammering refresh.
    LaunchedEffect(state.underReview) {
        while (state.underReview) {
            delay(10_000)
            viewModel.refresh(silent = true)
        }
    }

    val pickDocument = rememberLauncherForActivityResult(
        ActivityResultContracts.GetContent(),
    ) { uri: Uri? ->
        uri ?: return@rememberLauncherForActivityResult
        scope.launch {
            val (bytes, mime) = withContext(Dispatchers.IO) {
                val b = context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                b to (context.contentResolver.getType(uri) ?: "image/jpeg")
            }
            if (bytes != null) viewModel.uploadDocument(bytes, mime)
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp),
    ) {
        ScreenHeader(title = "Identity", onBack = onBack)

        when {
            state.loading -> Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(160.dp),
                contentAlignment = Alignment.Center,
            ) { CircularProgressIndicator(color = Rust) }

            state.error != null -> ErrorRetry(state.error ?: "") { viewModel.refresh() }

            state.verified -> StatusPanel(
                tint = PositiveGreen,
                title = "Verified",
                body = "Your identity is confirmed (level ${state.kycLevel}). " +
                    "You can send money and exchange currency.",
            )

            state.underReview -> StatusPanel(
                tint = Rust,
                title = "Under review",
                body = "We received your document and it's being checked. " +
                    "This usually takes less than a day — no need to resubmit.",
            )

            else -> {
                if (state.submissionStatus == "rejected") {
                    StatusPanel(
                        tint = NegativeRed,
                        title = "Verification declined",
                        body = "Your previous submission wasn't accepted. " +
                            "Check the document is sharp, complete and matches your name, then try again.",
                    )
                    Spacer(Modifier.height(20.dp))
                }

                Text(
                    "One-time identity check",
                    style = MaterialTheme.typography.titleLarge,
                    color = MaterialTheme.colorScheme.onBackground,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    "Enter your name exactly as in your document and attach a clear photo of it.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(20.dp))

                OutlinedTextField(
                    value = state.fullName,
                    onValueChange = viewModel::onNameChange,
                    label = { Text("Full name (as in document)") },
                    singleLine = true,
                    shape = RoundedCornerShape(14.dp),
                    colors = appFieldColors(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(14.dp))

                // Document kind: two honest options, no dropdown ceremony.
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    for ((value, label) in listOf("passport" to "Passport", "id_card" to "ID card")) {
                        val selected = state.documentType == value
                        OutlinedButton(
                            onClick = { viewModel.onDocumentType(value) },
                            shape = RoundedCornerShape(12.dp),
                            colors = androidx.compose.material3.ButtonDefaults.outlinedButtonColors(
                                containerColor = if (selected) Rust.copy(alpha = 0.16f) else MaterialTheme.colorScheme.surface,
                                contentColor = if (selected) Rust else MaterialTheme.colorScheme.onSurfaceVariant,
                            ),
                        ) { Text(label) }
                    }
                }
                Spacer(Modifier.height(14.dp))

                if (state.documentRef == null) {
                    PrimaryButton(
                        text = "Attach document photo",
                        onClick = { pickDocument.launch("image/*") },
                        loading = state.uploading,
                        modifier = Modifier.fillMaxWidth(),
                    )
                } else {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .background(PositiveGreen.copy(alpha = 0.12f), RoundedCornerShape(14.dp))
                            .padding(14.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            "Document attached",
                            style = MaterialTheme.typography.titleMedium,
                            fontSize = 14.sp,
                            color = PositiveGreen,
                            modifier = Modifier.weight(1f),
                        )
                        androidx.compose.material3.TextButton(
                            onClick = { pickDocument.launch("image/*") },
                        ) { Text("Replace", color = MaterialTheme.colorScheme.onSurfaceVariant) }
                    }
                }

                state.formError?.let {
                    Spacer(Modifier.height(10.dp))
                    Text(it, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
                }

                Spacer(Modifier.height(24.dp))
                PrimaryButton(
                    text = "Submit for review",
                    onClick = viewModel::submit,
                    enabled = state.canSubmit,
                    loading = state.submitting,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(24.dp))
            }
        }
    }
}

@Composable
private fun StatusPanel(
    tint: androidx.compose.ui.graphics.Color,
    title: String,
    body: String,
) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(tint.copy(alpha = 0.12f), RoundedCornerShape(16.dp))
            .padding(18.dp),
    ) {
        Text(title, style = MaterialTheme.typography.titleLarge, color = tint)
        Spacer(Modifier.height(6.dp))
        Text(
            body,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
