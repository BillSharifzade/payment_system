import java.net.URI
import java.net.URISyntaxException
import java.text.SimpleDateFormat
import java.util.Base64
import java.util.Date
import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.compose.compiler)
}

// --- Release signing (S3) ------------------------------------------------------
// The upload key never lives in the repo. It comes from mobile/keystore.properties
// (git-ignored; keys: storeFile, storePassword, keyAlias, keyPassword — storeFile
// relative to mobile/) or, on CI, from the environment: KEYSTORE_FILE,
// KEYSTORE_PASSWORD, KEY_ALIAS, KEY_PASSWORD. A prod release build without one
// FAILS (see the preProdReleaseBuild hook below) instead of quietly producing a
// debug-signed or unsigned artifact. dev/staging release builds stay debug-signed
// so they install straight onto test devices.
val keystoreProps = Properties().apply {
    val file = rootProject.file("keystore.properties")
    if (file.isFile) file.inputStream().use { load(it) }
}

fun signingValue(propertyKey: String, envKey: String): String? =
    keystoreProps.getProperty(propertyKey)?.trim()?.takeIf { it.isNotEmpty() }
        ?: System.getenv(envKey)?.trim()?.takeIf { it.isNotEmpty() }

val uploadStoreFile = signingValue("storeFile", "KEYSTORE_FILE")
val uploadStorePassword = signingValue("storePassword", "KEYSTORE_PASSWORD")
val uploadKeyAlias = signingValue("keyAlias", "KEY_ALIAS")
val uploadKeyPassword = signingValue("keyPassword", "KEY_PASSWORD")
val hasUploadKey = uploadStoreFile != null && uploadStorePassword != null &&
    uploadKeyAlias != null && uploadKeyPassword != null

// --- Certificate pinning (S1) ----------------------------------------------------
// `sha256/<base64 SPKI hash>` pins for the API host, comma-separated, enforced
// by OkHttp's CertificatePinner on every call (CertificatePins.kt). From a Gradle
// property (-PpaymentCertPins=…, or gradle.properties / ~/.gradle/gradle.properties)
// or the environment on CI:
//   prod:    paymentCertPins        / PAYMENT_CERT_PINS          (REQUIRED for a prod release)
//   staging: paymentCertPinsStaging / PAYMENT_CERT_PINS_STAGING  (optional; HTTPS staging only)
//   dev:     never pinned
// At least two distinct pins — the live key and an offline backup — or one key
// rotation bricks every installed app. Malformed values fail the build here.
fun gradleOrEnv(property: String, env: String): String =
    (project.findProperty(property) as String?)?.trim()?.takeIf { it.isNotEmpty() }
        ?: System.getenv(env)?.trim().orEmpty()

fun validatedPins(raw: String, source: String): String {
    if (raw.isEmpty()) return ""
    val pins = raw.split(',', ' ', '\n', '\t').map { it.trim() }.filter { it.isNotEmpty() }.distinct()
    for (pin in pins) {
        if (!pin.startsWith("sha256/")) throw GradleException("$source: certificate pin must start with sha256/: $pin")
        val hash = try {
            Base64.getDecoder().decode(pin.removePrefix("sha256/"))
        } catch (e: IllegalArgumentException) {
            throw GradleException("$source: certificate pin is not valid base64: $pin")
        }
        if (hash.size != 32) throw GradleException("$source: certificate pin is not a SHA-256 hash: $pin")
    }
    if (pins.size < 2) {
        throw GradleException("$source: at least two distinct certificate pins (the live key and an offline backup) are required")
    }
    return pins.joinToString(",")
}

val prodCertPins = validatedPins(gradleOrEnv("paymentCertPins", "PAYMENT_CERT_PINS"), "paymentCertPins")
val stagingCertPins = validatedPins(gradleOrEnv("paymentCertPinsStaging", "PAYMENT_CERT_PINS_STAGING"), "paymentCertPinsStaging")

// --- Staging endpoint --------------------------------------------------------------
// The deployed backend on srv-dchr01 (LAN), reached from a real phone on the same
// network. HTTP for now — a bare LAN IP can't get TLS. Override for a phone test
// against a developer box:
//   ./gradlew :app:assembleStagingRelease -PAPI_BASE_URL=http://192.168.5.105:8099
// Cleartext is permitted to THIS host only (a generated network-security-config,
// see generateStaging*NetworkSecurityConfig below) — not to every host.
val stagingBaseUrl: String = (project.findProperty("API_BASE_URL") as String?)
    ?.trim()?.takeIf { it.isNotEmpty() } ?: "http://192.168.1.156:8099"
val stagingUri: URI = try {
    URI(stagingBaseUrl)
} catch (e: URISyntaxException) {
    throw GradleException("API_BASE_URL is not a valid URL: '$stagingBaseUrl'", e)
}
val stagingHost: String = stagingUri.host
    ?.takeIf { Regex("^[A-Za-z0-9.-]+$").matches(it) }
    ?: throw GradleException("API_BASE_URL must be http(s)://<host or IPv4>[:port], got '$stagingBaseUrl'")
if (stagingUri.scheme != "http" && stagingUri.scheme != "https") {
    throw GradleException("API_BASE_URL must be http or https, got '$stagingBaseUrl'")
}
if (stagingCertPins.isNotEmpty() && stagingUri.scheme != "https") {
    throw GradleException("paymentCertPinsStaging is set but API_BASE_URL is not HTTPS: $stagingBaseUrl")
}

/**
 * Writes the staging flavor's network-security-config: cleartext denied
 * everywhere except the one configured API host (when that host is HTTP).
 */
abstract class StagingNetworkSecurityConfigTask : DefaultTask() {
    /** The host cleartext is allowed to; empty = none (an HTTPS staging URL). */
    @get:Input
    abstract val cleartextHost: Property<String>

    @get:OutputDirectory
    abstract val outputDirectory: DirectoryProperty

    @TaskAction
    fun write() {
        val dir = outputDirectory.get().dir("xml").asFile
        dir.mkdirs()
        val host = cleartextHost.get()
        val domainConfig = if (host.isEmpty()) {
            ""
        } else {
            "\n    <domain-config cleartextTrafficPermitted=\"true\">\n" +
                "        <domain includeSubdomains=\"false\">$host</domain>\n" +
                "    </domain-config>"
        }
        dir.resolve("network_security_config_staging.xml").writeText(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n" +
                "<!-- GENERATED by app/build.gradle.kts from API_BASE_URL — do not edit.\n" +
                "     staging: cleartext only to the configured API host. -->\n" +
                "<network-security-config>\n" +
                "    <base-config cleartextTrafficPermitted=\"false\" />" + domainConfig + "\n" +
                "</network-security-config>\n",
        )
    }
}

// --- Version code ----------------------------------------------------------------
// CI passes -PVERSION_CODE=<build number>; a local build defaults to today's date
// (yyMMdd) so a sideloaded build is always newer than yesterday's. Never reuse a
// versionCode that reached users: Play rejects it.
val versionCodeValue: Int = (project.findProperty("VERSION_CODE") as String?)
    ?.let { it.toIntOrNull() ?: error("VERSION_CODE must be an integer, got '$it'") }
    ?: SimpleDateFormat("yyMMdd").format(Date()).toInt()

android {
    namespace = "tj.payment.wallet"
    compileSdk = 36

    defaultConfig {
        applicationId = "tj.payment.wallet"
        minSdk = 26
        targetSdk = 36
        versionCode = versionCodeValue
        versionName = "0.1.0"
        // Which res/xml network-security-config the manifest uses (staging overrides).
        manifestPlaceholders["networkSecurityConfig"] = "@xml/network_security_config"
    }

    signingConfigs {
        // Always declared so `release` is a known name; only usable when a key is
        // configured. An unconfigured config is never assigned (see prod flavor).
        create("release") {
            if (hasUploadKey) {
                storeFile = rootProject.file(uploadStoreFile!!)
                storePassword = uploadStorePassword
                keyAlias = uploadKeyAlias
                keyPassword = uploadKeyPassword
                enableV1Signing = false
                enableV2Signing = true
                enableV3Signing = true
            }
        }
    }

    flavorDimensions += "env"
    productFlavors {
        create("dev") {
            dimension = "env"
            applicationIdSuffix = ".dev"
            versionNameSuffix = "-dev"
            // The emulator reaches services on the host machine at 10.0.2.2.
            // Cleartext to that one host is granted by src/dev/res/xml/network_security_config.xml.
            buildConfigField("String", "API_BASE_URL", "\"http://10.0.2.2:8099\"")
            // Screenshots allowed so the emulator QA loop (adb screencap) works.
            buildConfigField("boolean", "SECURE_WINDOW", "false")
            buildConfigField("String", "CERT_PINS", "\"\"")
            buildConfigField("boolean", "REQUIRE_CERT_PINS", "false")
            signingConfig = signingConfigs.getByName("debug")
        }
        create("staging") {
            dimension = "env"
            applicationIdSuffix = ".staging"
            versionNameSuffix = "-staging"
            // See "Staging endpoint" above: the URL, and cleartext to that host only.
            buildConfigField("String", "API_BASE_URL", "\"$stagingBaseUrl\"")
            buildConfigField("boolean", "SECURE_WINDOW", "false")
            buildConfigField("String", "CERT_PINS", "\"$stagingCertPins\"")
            buildConfigField("boolean", "REQUIRE_CERT_PINS", "false")
            manifestPlaceholders["networkSecurityConfig"] = "@xml/network_security_config_staging"
            signingConfig = signingConfigs.getByName("debug")
        }
        create("prod") {
            dimension = "env"
            // Placeholder until a real (public, TLS) server exists; HTTPS + pinning only.
            buildConfigField("String", "API_BASE_URL", "\"https://api.example.tj\"")
            // Enforced by OkHttp's CertificatePinner; a prod release refuses to
            // package without pins (preProdReleaseBuild below) and refuses to run
            // without them (AppContainer). Debug builds of prod are exempt.
            buildConfigField("String", "CERT_PINS", "\"$prodCertPins\"")
            buildConfigField("boolean", "REQUIRE_CERT_PINS", "true")
            // Real users' balances never appear in screenshots or app-switcher
            // thumbnails.
            buildConfigField("boolean", "SECURE_WINDOW", "true")
            // Release builds take the upload key; debug builds keep the debug key
            // (the debug build type's own signingConfig wins over the flavor's).
            signingConfig = if (hasUploadKey) signingConfigs.getByName("release") else null
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            // Explicit even though it is the default: a debuggable release build
            // exposes the app's memory (tokens, balances) to any USB-attached host.
            isDebuggable = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
            // No signingConfig here on purpose: it comes from the flavor (debug
            // key for dev/staging, upload key for prod).
        }
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

composeCompiler {
    // :core classes are immutable but cannot be annotated (no Compose dependency
    // there); this file declares them stable so rows/cards skip recomposition.
    stabilityConfigurationFile.set(layout.projectDirectory.file("compose_stability.conf"))
}

// staging: generate its network-security-config (cleartext to the configured
// API host only) into the variant's resources; the manifest points at it via
// the networkSecurityConfig placeholder set on the flavor.
androidComponents {
    onVariants { variant ->
        if (variant.flavorName != "staging") return@onVariants
        val taskName = "generate" + variant.name.replaceFirstChar { it.uppercase() } + "NetworkSecurityConfig"
        val generate = tasks.register<StagingNetworkSecurityConfigTask>(taskName) {
            cleartextHost.set(if (stagingUri.scheme == "http") stagingHost else "")
        }
        variant.sources.res?.addGeneratedSourceDirectory(generate, StagingNetworkSecurityConfigTask::outputDirectory)
    }
}

// A prod release build without an upload key or without certificate pins must
// not get as far as producing an artifact. preProdReleaseBuild is the first
// task of that variant, so this fails in milliseconds — before minutes of R8 —
// and only when the requested task graph would actually package something
// (unit tests and lint on the variant still run without them).
tasks.configureEach {
    if (name == "preProdReleaseBuild") {
        doFirst {
            val packaging = project.gradle.taskGraph.allTasks.any { task ->
                task.project == project &&
                    (task.name.startsWith("packageProdRelease") || task.name.startsWith("signProdRelease"))
            }
            if (packaging && !hasUploadKey) {
                throw GradleException(
                    "No upload key configured for the prod release build. Put the key in " +
                        "mobile/keystore.properties (storeFile, storePassword, keyAlias, keyPassword) " +
                        "or export KEYSTORE_FILE, KEYSTORE_PASSWORD, KEY_ALIAS and KEY_PASSWORD. " +
                        "Production builds are never signed with the debug key.",
                )
            }
            if (packaging && prodCertPins.isEmpty()) {
                throw GradleException(
                    "No certificate pins configured for the prod release build. Pass " +
                        "-PpaymentCertPins=sha256/<live key>,sha256/<offline backup key> (or export " +
                        "PAYMENT_CERT_PINS). Production builds never talk to the API unpinned.",
                )
            }
        }
    }
}

dependencies {
    implementation(project(":core"))

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.activity.compose)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.navigation.compose)
    debugImplementation(libs.androidx.compose.ui.tooling)

    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.okhttp)
    implementation(libs.kotlinx.serialization.json)
    implementation(libs.androidx.security.crypto)
    // Pay-by-QR: the device's biometric prompt (fingerprint / face / device
    // credential) authorises a check payment; ZXing draws the merchant's QR and
    // scans it (embedded: no Play Services needed on TJ's phones).
    implementation(libs.androidx.biometric)
    implementation(libs.androidx.fragment.ktx)
    implementation(libs.zxing.core)
    implementation(libs.zxing.android.embedded)

    // Installs a shipped Baseline Profile into ART at first launch so the hot
    // paths (Compose runtime, navigation, JSON decode) are AOT-compiled instead
    // of interpreted on the first runs — the single biggest cold-start win on
    // low-end phones. Harmless without a profile: nothing is installed.
    //
    // TODO(perf): generate the profile. Add a `:baselineprofile` macrobenchmark
    // module (androidx.benchmark.macro.junit4 + androidx.baselineprofile Gradle
    // plugin, the `androidx.baselineprofile` plugin also applied here with
    // `baselineProfile { automaticGenerationDuringBuild = false }`), write a
    // BaselineProfileRule test that drives launch → login → Home → Send/History,
    // run `./gradlew :app:generateBaselineProfile` on a rooted emulator or a
    // physical device, and commit the resulting
    // app/src/main/generated/baselineProfiles/baseline-prof.txt. Deliberately not
    // generated in this pass: it needs a device run, and a stale profile is worse
    // than none.
    implementation(libs.androidx.profileinstaller)

    testImplementation(libs.junit)
    testImplementation(libs.okhttp.mockwebserver)
    testImplementation(libs.okhttp.tls)
    testImplementation(libs.kotlinx.coroutines.test)
}
