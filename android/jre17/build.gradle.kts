/*
 * Java 17, delivered on demand by Google Play.
 *
 * 17 exists for one thing: Forge 1.20.1, which `Loader::java_policy` treats as
 * `Exact` and which therefore cannot run on 21 or 25 — modlauncher reaches into
 * `java.base` internals that a newer JDK has moved. Without a 17 staged, that
 * server is refused rather than hosted.
 *
 * **This one is never packaged with the app.** 21 and 25 ship install-time
 * today and 25 stays that way, because it runs the current Minecraft line and
 * the common case must not wait on a download. 17 is the opposite: a runtime a
 * minority of servers need, so it is downloaded when one selects it and never
 * otherwise. That asymmetry is the whole reason delivery is declared per module
 * rather than once for all of them.
 *
 * How this is wired, and why it is a feature module rather than an asset pack,
 * is in `docs/android-server-backend.md` § *Getting a JVM onto the device*.
 * This module carries no code of its own; its whole payload is
 * `src/main/assets/jre-17/`, staged by `npm run jre:android`.
 */
plugins {
    alias(libs.plugins.android.dynamic.feature)
}

android {
    namespace = "app.gethomerun.mobile.jre17"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
    }
}

dependencies {
    implementation(project(":app"))
}
