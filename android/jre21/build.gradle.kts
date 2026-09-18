/*
 * Java 21, delivered on demand by Google Play.
 *
 * The runtime is ~54 MB compressed and serves Minecraft 1.21.x and the mod
 * loaders, which break on a JDK newer than they were built against. Minecraft
 * 26.x needs 25 instead, so a device downloads whichever its servers select
 * and usually only one of the two.
 *
 * How this is wired, and why it is a feature module rather than an asset pack,
 * is in `docs/android-server-backend.md` § *Getting a JVM onto the device* —
 * along with what "promised but not present" costs the host. This module
 * carries no code of its own; its whole payload is
 * `src/main/assets/jre-21/`, staged by `npm run jre:android`.
 */
plugins {
    alias(libs.plugins.android.dynamic.feature)
}

android {
    namespace = "app.gethomerun.mobile.jre21"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
    }
}

dependencies {
    implementation(project(":app"))
}
