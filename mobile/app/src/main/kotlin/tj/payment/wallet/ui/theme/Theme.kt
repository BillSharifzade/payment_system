package tj.payment.wallet.ui.theme

import android.app.Activity
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import androidx.core.view.WindowCompat

// Warm-black + rust brand, shared with the ops console. Committed dark theme:
// a wallet reads as focused and premium in the dark, and it's the same identity
// across the operator console and the customer app.
val Rust = Color(0xFFD34516)
val RustBright = Color(0xFFF15A24)
private val BgBase = Color(0xFF0E0B0A)
private val BgSurface = Color(0xFF17110F)
private val BgElevated = Color(0xFF201814)
private val InkHigh = Color(0xFFF4EEEA)
private val InkMuted = Color(0xFFA79E98)
private val Line = Color(0xFF322721)
val PositiveGreen = Color(0xFF3FB27F)
val NegativeRed = Color(0xFFE5544B)

private val PaymentColors = darkColorScheme(
    primary = Rust,
    onPrimary = Color.White,
    primaryContainer = BgElevated,
    onPrimaryContainer = InkHigh,
    secondary = RustBright,
    background = BgBase,
    onBackground = InkHigh,
    surface = BgSurface,
    onSurface = InkHigh,
    surfaceVariant = BgElevated,
    onSurfaceVariant = InkMuted,
    outline = Line,
    error = NegativeRed,
    onError = Color.White,
)

private val PaymentTypography = Typography(
    headlineMedium = TextStyle(fontWeight = FontWeight.SemiBold, fontSize = 26.sp, letterSpacing = (-0.5).sp),
    titleLarge = TextStyle(fontWeight = FontWeight.SemiBold, fontSize = 20.sp),
    titleMedium = TextStyle(fontWeight = FontWeight.Medium, fontSize = 16.sp),
    bodyLarge = TextStyle(fontWeight = FontWeight.Normal, fontSize = 16.sp),
    bodyMedium = TextStyle(fontWeight = FontWeight.Normal, fontSize = 14.sp, color = InkMuted),
    labelLarge = TextStyle(fontWeight = FontWeight.SemiBold, fontSize = 15.sp),
)

@Composable
fun PaymentTheme(content: @Composable () -> Unit) {
    val view = LocalView.current
    if (!view.isInEditMode) {
        SideEffect {
            val window = (view.context as Activity).window
            WindowCompat.getInsetsController(window, view).isAppearanceLightStatusBars =
                PaymentColors.background.luminance() > 0.5f
        }
    }
    // isSystemInDarkTheme referenced so lint sees intentional single-theme choice.
    @Suppress("UNUSED_VARIABLE") val ignored = isSystemInDarkTheme()
    MaterialTheme(
        colorScheme = PaymentColors,
        typography = PaymentTypography,
        content = content,
    )
}
