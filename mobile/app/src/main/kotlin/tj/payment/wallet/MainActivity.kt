package tj.payment.wallet

import android.os.Build
import android.os.Bundle
import android.view.Display
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import tj.payment.wallet.ui.AppRoot
import tj.payment.wallet.ui.theme.PaymentTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        requestHighestRefreshRate()

        // Balances and payment details never appear in screenshots, screen
        // recordings, or the recent-apps switcher on production builds. Test
        // flavors keep screenshots so the emulator QA loop still works.
        if (BuildConfig.SECURE_WINDOW) {
            window.setFlags(
                android.view.WindowManager.LayoutParams.FLAG_SECURE,
                android.view.WindowManager.LayoutParams.FLAG_SECURE,
            )
        }

        val container = (application as PaymentApp).container
        setContent {
            PaymentTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    androidx.compose.foundation.layout.Box(Modifier.systemBarsPadding()) {
                        AppRoot(container)
                    }
                }
            }
        }
    }

    /**
     * Opt the window into the display's fastest mode at the current resolution.
     * Without this, many phones (Xiaomi/MIUI especially) render apps at 60 Hz even
     * on a 120 Hz panel — the difference the user feels as "not smooth".
     */
    private fun requestHighestRefreshRate() {
        @Suppress("DEPRECATION")
        val display: Display? =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display else windowManager.defaultDisplay
        display ?: return

        val current = display.mode
        val fastest = display.supportedModes
            .filter {
                it.physicalWidth == current.physicalWidth &&
                    it.physicalHeight == current.physicalHeight
            }
            .maxByOrNull { it.refreshRate } ?: return

        if (fastest.modeId != current.modeId) {
            window.attributes = window.attributes.apply {
                preferredDisplayModeId = fastest.modeId
            }
        }
    }
}
