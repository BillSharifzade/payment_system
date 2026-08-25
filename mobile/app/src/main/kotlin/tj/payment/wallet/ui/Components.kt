package tj.payment.wallet.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
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
