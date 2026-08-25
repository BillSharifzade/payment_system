plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.compose.compiler)
}

android {
    namespace = "tj.payment.wallet"
    compileSdk = 36

    defaultConfig {
        applicationId = "tj.payment.wallet"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    flavorDimensions += "env"
    productFlavors {
        create("dev") {
            dimension = "env"
            applicationIdSuffix = ".dev"
            versionNameSuffix = "-dev"
            // The emulator reaches services on the host machine at 10.0.2.2.
            buildConfigField("String", "API_BASE_URL", "\"http://10.0.2.2:8099\"")
            buildConfigField("boolean", "ALLOW_CLEARTEXT", "true")
            // Screenshots allowed so the emulator QA loop (adb screencap) works.
            buildConfigField("boolean", "SECURE_WINDOW", "false")
        }
        create("staging") {
            dimension = "env"
            applicationIdSuffix = ".staging"
            versionNameSuffix = "-staging"
            // The deployed backend on srv-dchr01 (LAN), reached from a real phone
            // on the same network. HTTP for now — a bare LAN IP can't get TLS.
            buildConfigField("String", "API_BASE_URL", "\"http://192.168.1.156:8099\"")
            buildConfigField("boolean", "ALLOW_CLEARTEXT", "true")
            buildConfigField("boolean", "SECURE_WINDOW", "false")
        }
        create("prod") {
            dimension = "env"
            // Placeholder until a real (public, TLS) server exists; HTTPS + pinning only.
            buildConfigField("String", "API_BASE_URL", "\"https://api.example.tj\"")
            buildConfigField("boolean", "ALLOW_CLEARTEXT", "false")
            // Real users' balances never appear in screenshots or app-switcher
            // thumbnails.
            buildConfigField("boolean", "SECURE_WINDOW", "true")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
            // Signed with the debug key so release builds install directly for
            // on-device testing (real smoothness needs debuggable=false + R8).
            // Replace with a real upload key before any Play/production release.
            signingConfig = signingConfigs.getByName("debug")
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
}
