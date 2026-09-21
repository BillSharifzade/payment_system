import java.text.SimpleDateFormat
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
            signingConfig = signingConfigs.getByName("debug")
        }
        create("staging") {
            dimension = "env"
            applicationIdSuffix = ".staging"
            versionNameSuffix = "-staging"
            // The deployed backend on srv-dchr01 (LAN), reached from a real phone
            // on the same network. HTTP for now — a bare LAN IP can't get TLS;
            // granted by src/staging/res/xml/network_security_config.xml.
            buildConfigField("String", "API_BASE_URL", "\"http://192.168.1.156:8099\"")
            buildConfigField("boolean", "SECURE_WINDOW", "false")
            signingConfig = signingConfigs.getByName("debug")
        }
        create("prod") {
            dimension = "env"
            // Placeholder until a real (public, TLS) server exists; HTTPS + pinning only.
            buildConfigField("String", "API_BASE_URL", "\"https://api.example.tj\"")
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

// A prod release build without an upload key must not get as far as producing
// an artifact. preProdReleaseBuild is the first task of that variant, so this
// fails in milliseconds — before minutes of R8 — and only when the requested
// task graph would actually package something (unit tests and lint on the
// variant still run without a key).
tasks.configureEach {
    if (name == "preProdReleaseBuild" && !hasUploadKey) {
        doFirst {
            val packaging = project.gradle.taskGraph.allTasks.any { task ->
                task.project == project &&
                    (task.name.startsWith("packageProdRelease") || task.name.startsWith("signProdRelease"))
            }
            if (packaging) {
                throw GradleException(
                    "No upload key configured for the prod release build. Put the key in " +
                        "mobile/keystore.properties (storeFile, storePassword, keyAlias, keyPassword) " +
                        "or export KEYSTORE_FILE, KEYSTORE_PASSWORD, KEY_ALIAS and KEY_PASSWORD. " +
                        "Production builds are never signed with the debug key.",
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
    testImplementation(libs.kotlinx.coroutines.test)
}
