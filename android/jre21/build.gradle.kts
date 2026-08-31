/*
 * Java 21, delivered on demand by Google Play.
 *
 * The runtime is ~54 MB compressed and most players never need it: 21 exists
 * for the mod loaders, which break on a JDK newer than they were built
 * against, while everything else runs on the 25 that ships in the base APK.
 * Carrying both put the install at ~167 MB of Play's 200 MB ceiling, and an
 * on-demand feature module is not counted against it at all.
 *
 * It is a *feature* module and not an asset pack, which is the whole reason
 * this module has a build script rather than an `assetPack {}` block: asset
 * packs are "composed of assets ... but no executable code", and a JRE is
 * `libjvm.so` and friends. Play Feature Delivery is the sanctioned way to
 * deliver code, and it still comes from Play — so the Device and Network
 * Abuse rule that keeps the runtime out of a plain download is satisfied too.
 * See `docs/android-server-backend.md` § *Getting a JVM onto the device*.
 *
 * The module carries no code of its own. Its whole payload is
 * `src/main/assets/jre-21/`, staged by `npm run jre:android` exactly as the
 * base APK's runtime is, so `JavaRuntime` reads it through the same
 * `AssetManager` once SplitCompat has merged the split in.
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
