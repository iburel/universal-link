import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release signing. The keystore and its passwords come from the ENVIRONMENT, never
// from a file in the tree: release.yml decodes the keystore out of a repository
// secret into the runner's temp directory (outside the checkout, so no artifact
// glob can reach it) and exports these four variables. The Tauri template's
// `keystore.properties` convention is the alternative, and it is worse — it puts
// the store password in plaintext inside gen/android, gitignored but still on disk
// and one `git add -f` away from being published.
//
// `providers.environmentVariable` and NOT `System.getenv`: the latter reads the
// GRADLE DAEMON's environment, i.e. whichever client happened to start it. Build
// once without the variables and the daemon caches their absence, so exporting them
// and rebuilding would keep quietly producing an unsigned APK. The provider is
// resolved against this build's own environment and is a tracked build input.
val signingEnv = listOf(
    "ANDROID_KEYSTORE_PATH",
    "ANDROID_KEYSTORE_PASSWORD",
    "ANDROID_KEY_ALIAS",
    "ANDROID_KEY_PASSWORD",
).associateWith { providers.environmentVariable(it).orNull?.takeIf { v -> v.isNotEmpty() } }

// Half a configuration is a misconfiguration, and it MUST NOT degrade to an
// unsigned build: a mistyped secret name in the workflow would otherwise sail
// through and ship an unsigned APK. Fail at configuration time instead.
val signRelease = signingEnv.values.any { it != null }
if (signRelease) {
    val missing = signingEnv.filterValues { it == null }.keys
    require(missing.isEmpty()) {
        "Release signing is half-configured: set all four of ${signingEnv.keys}, missing $missing"
    }
}

android {
    compileSdk = 36
    namespace = "org.universallink.mobile"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "org.universallink.mobile"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (signRelease) {
            create("release") {
                val keystore = file(signingEnv.getValue("ANDROID_KEYSTORE_PATH")!!)
                require(keystore.isFile) {
                    "ANDROID_KEYSTORE_PATH does not point at a file: $keystore"
                }
                storeFile = keystore
                storePassword = signingEnv.getValue("ANDROID_KEYSTORE_PASSWORD")
                keyAlias = signingEnv.getValue("ANDROID_KEY_ALIAS")
                keyPassword = signingEnv.getValue("ANDROID_KEY_PASSWORD")
                // minSdk is 24, which is exactly where the v1 (JAR) scheme stops
                // being consulted — so keeping it buys nothing and costs: v1 is the
                // scheme Janus (CVE-2017-13156) attacks, because it signs entries
                // rather than the file, letting a DEX blob be prepended to a
                // still-"valid" APK. v2 and v3 cover the archive as a whole.
                enableV1Signing = false
                enableV2Signing = true
                enableV3Signing = true
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            // Left unset when the signing environment is absent — every local build
            // and ci.yml's release-APK gate. AGP then names the output
            // `app-universal-release-unsigned.apk` instead of
            // `app-universal-release.apk`, which is what release.yml's signature
            // check keys off. It does NOT fall back to the debug key: that fallback
            // exists for the `debug` buildType alone.
            if (signRelease) signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

// These raise the Kotlin floor: activity, lifecycle-process and webkit declare
// kotlin-stdlib 2.1.20, so the Kotlin plugin in ../build.gradle.kts has to be
// >= 2.0 to read their metadata at all — see the ceiling documented there.
// compileSdk 36 also becomes a hard floor here (activity 1.13.0 and the
// androidx.core 1.18.0 it pulls both require it); we are already at 36.
dependencies {
    implementation("androidx.webkit:webkit:1.16.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.13.0")
    implementation("com.google.android.material:material:1.14.0")
    implementation("androidx.lifecycle:lifecycle-process:2.11.0")
    // The QR scanner (ScanActivity): the camera, and the decoder. Both Apache-2.0
    // — the alternative was Google's code scanner, and ScanBridge.kt says why an
    // AGPL app that must also run on a de-Googled phone does not take it.
    //
    // camera-view is what makes this four artifacts rather than two: it carries
    // PreviewView, which owns the surface's lifecycle and its rotation. CameraX
    // 1.5 declares kotlin-stdlib 2.0.21, well under the metadata ceiling the root
    // build.gradle.kts documents.
    implementation("androidx.camera:camera-core:1.5.3")
    implementation("androidx.camera:camera-camera2:1.5.3")
    implementation("androidx.camera:camera-lifecycle:1.5.3")
    implementation("androidx.camera:camera-view:1.5.3")
    // `core` alone: the decoder, pure Java, no Android dependency and no UI. The
    // `android-core`/`zxing-android-embedded` packages bring a capture activity
    // built on the deprecated Camera1 API, which is what CameraX above replaces.
    implementation("com.google.zxing:core:3.5.4")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.7.0")
}

apply(from = "tauri.build.gradle.kts")