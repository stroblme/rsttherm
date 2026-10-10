// The viewer is built by cargo, not by Gradle; see `app/build.gradle.kts`, which drives it.
plugins {
    alias(libs.plugins.android.application) apply false
}
