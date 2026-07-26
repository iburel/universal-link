buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:8.11.0")
        // 2.2.21 and NOT NEWER. This is a ceiling, not a preference.
        //
        // Why it moved off the template's 1.9.25 at all: the androidx artifacts in
        // app/build.gradle.kts now ship Kotlin 2.1.0 metadata (activity 1.13.0,
        // lifecycle-process 2.11.0, core 1.18.0, webkit 1.16.0 — the last three
        // declare kotlin-stdlib 2.1.20), and a Kotlin compiler reads metadata only
        // up to its own version plus one minor. 1.9.25 tops out at 2.0.0, so it
        // fails the whole compileKotlin task with "Incompatible classes were found
        // in dependencies", cascading into our own sources as bogus
        // "Unresolved reference" errors — enableEdgeToEdge, takeIf, even ArrayList.
        //
        // Why not 2.3 or 2.4: from 2.3 the Kotlin plugin turns
        // `kotlinOptions { jvmTarget = "..." }` into a HARD ERROR (migrate to the
        // compilerOptions DSL). Fixing app/build.gradle.kts would not be enough —
        // the same construct lives in the Gradle scripts of tauri and
        // tauri-plugin-opener, which Gradle reads straight out of
        // ~/.cargo/registry and CI re-downloads on every run. We cannot patch
        // those. Bisected: 2.3.21 red, 2.2.21 green, and 2.2.21 is the newest
        // published 2.2.x.
        //
        // AGP stays 8.11.0 and the wrapper stays 8.14.3 on purpose; KGP 2.2.x is
        // tested against Gradle 7.6.3-8.14 and AGP 7.3.1-8.11.1, so this is inside
        // the supported window with nothing else to move. Going to AGP 9 / Gradle 9
        // is a different and currently IMPOSSIBLE change: Gradle 9 removed
        // `Project.exec`, which buildSrc/.../BuildTask.kt uses, and that file is the
        // one file `cargo tauri android init` force-rewrites on every run
        // (tauri-cli src/mobile/android/project.rs) — so it cannot be patched here.
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.4.10")
    }
}

allprojects {
    repositories {
        google()
        mavenCentral()
    }
}

tasks.register("clean").configure {
    delete("build")
}

