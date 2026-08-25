package tj.payment.wallet.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tj.payment.wallet.ui.theme.NegativeRed
import tj.payment.wallet.ui.theme.PositiveGreen
import tj.payment.wallet.ui.theme.Rust
import tj.payment.wallet.ui.theme.RustBright

/**
 * Full-width primary action button with a subtle press-scale — the tactile
 * feedback that makes taps feel responsive on a high-refresh screen.
 */
@Composable
fun PrimaryButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    loading: Boolean = false,
) {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(
        targetValue = if (pressed) 0.97f else 1f,
        animationSpec = spring(dampingRatio = 0.55f, stiffness = 900f),
        label = "pressScale",
    )

    Button(
        onClick = onClick,
        enabled = enabled && !loading,
        interactionSource = interaction,
        shape = RoundedCornerShape(16.dp),
        colors = ButtonDefaults.buttonColors(
            containerColor = Rust,
            contentColor = Color.White,
        ),
        modifier = modifier
            .height(54.dp)
            .graphicsLayer {
                scaleX = scale
                scaleY = scale
            },
    ) {
        if (loading) {
            CircularProgressIndicator(
                modifier = Modifier.size(22.dp),
                strokeWidth = 2.dp,
                color = Color.White,
            )
        } else {
            Text(text, style = MaterialTheme.typography.labelLarge)
        }
    }
}

/**
 * The app's brand mark: a rounded rust tile with a minimal coin glyph. Vector-
 * drawn (no image asset), so it's crisp at any density.
 */
@Composable
fun BrandMark(modifier: Modifier = Modifier, size: Int = 60) {
    Box(
        modifier = modifier
            .size(size.dp)
            .graphicsLayer { clip = false },
        contentAlignment = Alignment.Center,
    ) {
        Canvas(Modifier.size(size.dp)) {
            val s = this.size.minDimension
            val corner = s * 0.28f
            drawRoundRect(
                brush = Brush.linearGradient(
                    colors = listOf(RustBright, Rust),
                    start = Offset(0f, 0f),
                    end = Offset(s, s),
                ),
                cornerRadius = androidx.compose.ui.geometry.CornerRadius(corner, corner),
            )
            // Upward arrow — clean, positive, reads as "send/pay".
            val cx = s / 2f
            val stroke = s * 0.09f
            val top = s * 0.30f
            val bottom = s * 0.70f
            val head = s * 0.17f
            drawLine(
                color = Color.White,
                start = Offset(cx, bottom),
                end = Offset(cx, top),
                strokeWidth = stroke,
                cap = StrokeCap.Round,
            )
            drawLine(
                color = Color.White,
                start = Offset(cx, top),
                end = Offset(cx - head, top + head),
                strokeWidth = stroke,
                cap = StrokeCap.Round,
            )
            drawLine(
                color = Color.White,
                start = Offset(cx, top),
                end = Offset(cx + head, top + head),
                strokeWidth = stroke,
                cap = StrokeCap.Round,
            )
        }
    }
}

/** Header for inner screens: back chevron + title, consistent everywhere. */
@Composable
fun ScreenHeader(title: String, onBack: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 18.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(40.dp)
                .background(MaterialTheme.colorScheme.surface, CircleShape)
                .clickable(onClick = onBack),
            contentAlignment = Alignment.Center,
        ) {
            Chevron(back = true, color = MaterialTheme.colorScheme.onSurface)
        }
        Spacer(Modifier.width(14.dp))
        Text(
            text = title,
            style = MaterialTheme.typography.headlineMedium,
            color = MaterialTheme.colorScheme.onBackground,
        )
    }
}

@Composable
private fun Chevron(back: Boolean, color: Color) {
    Canvas(Modifier.size(16.dp)) {
        val s = size.minDimension
        val stroke = s * 0.14f
        val midY = s / 2f
        val tipX = if (back) s * 0.32f else s * 0.68f
        val backX = if (back) s * 0.68f else s * 0.32f
        drawLine(color, Offset(backX, s * 0.14f), Offset(tipX, midY), stroke, StrokeCap.Round)
        drawLine(color, Offset(tipX, midY), Offset(backX, s * 0.86f), stroke, StrokeCap.Round)
    }
}

/**
 * Direction glyph for a statement row: a circled arrow. Up-right & red-tinted
 * for money out, down-left & green-tinted for money in, both-ways for FX.
 * Shape + color together (never color alone) carry the meaning.
 */
@Composable
fun DirectionBadge(kind: String, isCredit: Boolean, modifier: Modifier = Modifier) {
    val accent = when {
        kind == "fx" -> RustBright
        isCredit -> PositiveGreen
        else -> NegativeRed
    }
    Box(
        modifier = modifier
            .size(40.dp)
            .background(accent.copy(alpha = 0.14f), CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Canvas(Modifier.size(18.dp)) {
            val s = size.minDimension
            val stroke = s * 0.14f
            if (kind == "fx") {
                // Two opposing horizontal arrows: an exchange.
                val yTop = s * 0.30f
                val yBot = s * 0.70f
                drawLine(accent, Offset(s * 0.12f, yTop), Offset(s * 0.88f, yTop), stroke, StrokeCap.Round)
                drawLine(accent, Offset(s * 0.60f, yTop - s * 0.16f), Offset(s * 0.88f, yTop), stroke, StrokeCap.Round)
                drawLine(accent, Offset(s * 0.60f, yTop + s * 0.16f), Offset(s * 0.88f, yTop), stroke, StrokeCap.Round)
                drawLine(accent, Offset(s * 0.88f, yBot), Offset(s * 0.12f, yBot), stroke, StrokeCap.Round)
                drawLine(accent, Offset(s * 0.40f, yBot - s * 0.16f), Offset(s * 0.12f, yBot), stroke, StrokeCap.Round)
                drawLine(accent, Offset(s * 0.40f, yBot + s * 0.16f), Offset(s * 0.12f, yBot), stroke, StrokeCap.Round)
            } else {
                // Diagonal arrow: out = up-right, in = down-left.
                val from = if (isCredit) Offset(s * 0.78f, s * 0.22f) else Offset(s * 0.22f, s * 0.78f)
                val to = if (isCredit) Offset(s * 0.22f, s * 0.78f) else Offset(s * 0.78f, s * 0.22f)
                drawLine(accent, from, to, stroke, StrokeCap.Round)
                val headA = if (isCredit) Offset(s * 0.22f, s * 0.42f) else Offset(s * 0.78f, s * 0.58f)
                val headB = if (isCredit) Offset(s * 0.58f, s * 0.78f) else Offset(s * 0.42f, s * 0.22f)
                drawLine(accent, headA, to, stroke, StrokeCap.Round)
                drawLine(accent, headB, to, stroke, StrokeCap.Round)
            }
        }
    }
}

/** A circular home-screen action: glyph tile + label underneath. */
@Composable
fun ActionButton(
    label: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    glyph: @Composable () -> Unit,
) {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(
        targetValue = if (pressed) 0.92f else 1f,
        animationSpec = spring(dampingRatio = 0.55f, stiffness = 900f),
        label = "actionScale",
    )
    Column(
        // The whole tile — glyph AND label — is the touch target; a label that
        // ignores taps reads as broken.
        modifier = modifier.clickable(
            interactionSource = interaction,
            indication = null,
            onClick = onClick,
        ),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            modifier = Modifier
                .size(58.dp)
                .graphicsLayer {
                    scaleX = scale
                    scaleY = scale
                }
                .background(MaterialTheme.colorScheme.surfaceVariant, CircleShape),
            contentAlignment = Alignment.Center,
        ) { glyph() }
        Spacer(Modifier.height(8.dp))
        Text(
            label,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** Simple stroke glyphs for the home actions, drawn in the brand rust. */
@Composable
fun GlyphArrow(up: Boolean) {
    val color = Rust
    Canvas(Modifier.size(22.dp)) {
        val s = size.minDimension
        val stroke = s * 0.12f
        val cx = s / 2f
        val tip = if (up) s * 0.18f else s * 0.82f
        val tail = if (up) s * 0.82f else s * 0.18f
        val head = s * 0.24f
        drawLine(color, Offset(cx, tail), Offset(cx, tip), stroke, StrokeCap.Round)
        drawLine(color, Offset(cx - head, if (up) tip + head else tip - head), Offset(cx, tip), stroke, StrokeCap.Round)
        drawLine(color, Offset(cx + head, if (up) tip + head else tip - head), Offset(cx, tip), stroke, StrokeCap.Round)
    }
}

@Composable
fun GlyphList() {
    val color = Rust
    Canvas(Modifier.size(22.dp)) {
        val s = size.minDimension
        val stroke = s * 0.11f
        for ((i, y) in listOf(0.24f, 0.5f, 0.76f).withIndex()) {
            drawCircle(color, radius = stroke * 0.6f, center = Offset(s * 0.14f, s * y))
            drawLine(color, Offset(s * 0.32f, s * y), Offset(s * (if (i == 1) 0.86f else 0.74f), s * y), stroke, StrokeCap.Round)
        }
    }
}

@Composable
fun GlyphSwap() {
    val color = Rust
    Canvas(Modifier.size(22.dp)) {
        val s = size.minDimension
        val stroke = s * 0.12f
        val yTop = s * 0.32f
        val yBot = s * 0.68f
        drawLine(color, Offset(s * 0.14f, yTop), Offset(s * 0.86f, yTop), stroke, StrokeCap.Round)
        drawLine(color, Offset(s * 0.62f, yTop - s * 0.15f), Offset(s * 0.86f, yTop), stroke, StrokeCap.Round)
        drawLine(color, Offset(s * 0.62f, yTop + s * 0.15f), Offset(s * 0.86f, yTop), stroke, StrokeCap.Round)
        drawLine(color, Offset(s * 0.86f, yBot), Offset(s * 0.14f, yBot), stroke, StrokeCap.Round)
        drawLine(color, Offset(s * 0.38f, yBot - s * 0.15f), Offset(s * 0.14f, yBot), stroke, StrokeCap.Round)
        drawLine(color, Offset(s * 0.38f, yBot + s * 0.15f), Offset(s * 0.14f, yBot), stroke, StrokeCap.Round)
    }
}

/** Inline error line with a retry action — the standard failure state. */
@Composable
fun ErrorRetry(message: String, onRetry: () -> Unit) {
    Column {
        Text(message, color = NegativeRed, style = MaterialTheme.typography.bodyMedium)
        Spacer(Modifier.height(4.dp))
        TextButton(onClick = onRetry) { Text("Retry", color = Rust) }
    }
}

/** A key/value line on confirm screens: label left, value right. */
@Composable
fun KeyValueRow(label: String, value: String, valueColor: Color = Color.Unspecified) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.weight(1f))
        Text(
            value,
            style = MaterialTheme.typography.titleMedium,
            fontSize = 15.sp,
            color = if (valueColor == Color.Unspecified) MaterialTheme.colorScheme.onSurface else valueColor,
        )
    }
}

/** Shared rust-accented text-field colors (the Login style, reused app-wide). */
@Composable
fun appFieldColors() = OutlinedTextFieldDefaults.colors(
    focusedBorderColor = Rust,
    focusedLabelColor = Rust,
    cursorColor = Rust,
)
