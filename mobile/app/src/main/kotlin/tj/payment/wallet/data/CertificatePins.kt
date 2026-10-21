package tj.payment.wallet.data

import java.util.Base64
import okhttp3.CertificatePinner
import okhttp3.HttpUrl.Companion.toHttpUrl

/**
 * Certificate pinning for the API host (FRONTEND.md §3 hardening). The pins
 * come from the build (`-PpaymentCertPins=sha256/AAA…=,sha256/BBB…=`, per
 * flavor, into `BuildConfig.CERT_PINS`), are validated here, and are enforced
 * by OkHttp's [CertificatePinner] on every ApiClient call, the token refresh
 * included. The platform network-security-config is not used for pins: its
 * `<pin-set>` cannot take build-time values.
 *
 * Rules:
 *  - every pin is `sha256/` + the base64 SHA-256 of a SubjectPublicKeyInfo;
 *  - at least TWO distinct pins — the live key and an offline backup key — or
 *    one key rotation bricks every installed app;
 *  - pins only make sense over HTTPS.
 *
 * Compute a pin with
 * `openssl s_client -connect HOST:443 -servername HOST </dev/null 2>/dev/null | openssl x509 -pubkey -noout
 *  | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64`.
 */
object CertificatePins {
    const val MIN_PINS = 2

    /** Comma/whitespace-separated pins as configured; blanks dropped. */
    fun parse(raw: String): List<String> =
        raw.split(',', ' ', '\n', '\t').map { it.trim() }.filter { it.isNotEmpty() }

    /** @throws IllegalArgumentException with a precise reason when [pins] is unusable. */
    fun validate(pins: List<String>) {
        for (pin in pins) {
            require(pin.startsWith("sha256/")) { "certificate pin must start with sha256/: $pin" }
            val hash = try {
                Base64.getDecoder().decode(pin.removePrefix("sha256/"))
            } catch (e: IllegalArgumentException) {
                throw IllegalArgumentException("certificate pin is not valid base64: $pin", e)
            }
            require(hash.size == 32) { "certificate pin is not a SHA-256 hash (${hash.size} bytes): $pin" }
        }
        require(pins.distinct().size >= MIN_PINS) {
            "need at least $MIN_PINS distinct certificate pins (the live key and an offline backup), got ${pins.distinct().size}"
        }
    }

    /**
     * The pinner for [baseUrl]'s host, or null when [pins] is empty (no pinning:
     * dev, local staging). Fails fast on a bad configuration — a release that
     * pins wrongly must not get as far as a user.
     */
    fun pinnerFor(baseUrl: String, pins: List<String>): CertificatePinner? {
        if (pins.isEmpty()) return null
        validate(pins)
        val url = baseUrl.toHttpUrl()
        require(url.isHttps) { "certificate pins configured for a non-HTTPS API URL: $baseUrl" }
        return CertificatePinner.Builder().add(url.host, *pins.distinct().toTypedArray()).build()
    }
}
