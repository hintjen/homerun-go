/*
 * Java 25, delivered on demand by Google Play.
 *
 * 25 runs Minecraft 26.x, the current release line, so this is the more
 * commonly selected of the two runtimes — and it is still not in the base APK.
 * Both packaged put the install at ~167 MB of Play's 200 MB ceiling, and
 * neither runtime is wanted at all by a device that only hosts Pumpkin, which
 * needs no JVM. Deferring both drops the base install to ~54 MB.
 *
 * How this is wired, and why it is a feature module rather than an asset pack,
 * is in `docs/android-server-backend.md` § *Getting a JVM onto the device* —
 * along with what "promised but not present" costs the host. This module
 * carries no code of its own; its whole payload is `src/main/assets/jre-25/`,
 * staged by `npm run jre:android`.
 */
plugins {
    alias(libs.plugins.android.dynamic.feature)
}

android {
    namespace = "app.gethomerun.mobile.jre25"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
    }
}

dependencies {
    implementation(project(":app"))
}
