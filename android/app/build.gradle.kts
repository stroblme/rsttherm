plugins {
    alias(libs.plugins.android.application)
}

// The repository root: the crate whose viewer this APK runs.
val repo: File = rootProject.projectDir.parentFile

android {
    namespace = "io.github.stroblme.rsttherm"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.github.stroblme.rsttherm"
        minSdk = 26
        // Sideload-only, so no store forces a newer one. 35 enforces edge-to-edge, and a
        // NativeActivity gets no insets to keep the viewer clear of the bars (egui's safe-area
        // support is iOS-only).
        targetSdk = 34
    }

    packaging {
        // Uncompressed and page-aligned in the APK, which a 16 KB device needs to map the
        // library at all.
        jniLibs { useLegacyPackaging = false }
    }

    // A release is signed with the keystore these name, as CI does for a tag
    // (.github/workflows/ci.yml); without them assembleRelease leaves the APK unsigned. One key
    // for every release, so each installs over the last.
    val keystore = System.getenv("RSTTHERM_KEYSTORE")
    if (keystore != null) {
        val release = signingConfigs.create("release") {
            storeFile = file(keystore)
            storePassword = System.getenv("RSTTHERM_KEYSTORE_PASSWORD")
            keyAlias = "rsttherm"
            // A PKCS12 keystore, keytool's default, has one password for both.
            keyPassword = storePassword
        }
        buildTypes.getByName("release").signingConfig = release
    }
}

// The viewer, cross-compiled into `src/main/jniLibs/<abi>/`. Gradle runs the same `make` target
// a developer would.
val cargoNdk = tasks.register<Exec>("cargoNdk") {
    workingDir = repo
    commandLine("make", "android")
    inputs.dir(repo.resolve("src"))
    inputs.dir(repo.resolve("android/native"))
    // `files`, not `file`: Cargo.lock is not in git, so a fresh clone has none until cargo runs.
    inputs.files(repo.resolve("Cargo.toml"), repo.resolve("Cargo.lock"))
    outputs.dir(repo.resolve("android/app/src/main/jniLibs"))
}

tasks.named("preBuild") { dependsOn(cargoNdk) }
