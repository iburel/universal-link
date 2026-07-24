// Kotlin support is built into AGP >= 9 — declaring org.jetbrains.kotlin.android
// here is a fatal error. Only the application plugin is needed.
plugins {
    id("com.android.application") version "9.3.1"
}

android {
    namespace = "dev.universallink.app"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.universallink.app"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0-brick1"
        // arm64-v8a only for now: the test phone (and the .so cargo-ndk builds).
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    // The native .so is dropped into src/main/jniLibs/<abi>/ by cargo-ndk.
}
