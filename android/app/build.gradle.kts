// Imported rather than written out: inside this script `java` is the Java
// plugin extension, so `java.util.Properties` does not resolve.
import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.serialization)
}

/** A `-P` override, or the default. Keeps build config out of source. */
fun prop(name: String, fallback: String): String =
    (project.findProperty(name) as String?)?.takeIf { it.isNotBlank() } ?: fallback

/**
 * The one ABI this build packages, or null for "every ABI that is staged".
 *
 * Read once, because two things depend on it and they have to agree: which
 * native libraries are packaged, and which Java runtime was staged. When they
 * disagree the app installs and runs and simply cannot host — see
 * `verifyJavaRuntime`.
 */
val requestedAbi: String? = (project.findProperty("abi") as String?)?.takeIf { it.isNotBlank() }

/**
 * Per ABI: the `OS_ARCH` its Java runtime reports in `assets/jre/release`, and
 * the command that stages one. Both belong to `scripts/stage-jre.py`, the only
 * thing that ever writes into `assets/jre/`; it takes the same two ABI names.
 */
val jreForAbi = mapOf(
    "arm64-v8a" to Pair("aarch64", "npm run jre:android"),
    "x86_64" to Pair("x86_64", "npm run jre:android-x86_64"),
)

android {
    namespace = "app.gethomerun.mobile"
    compileSdk = 35

    defaultConfig {
        applicationId = "app.gethomerun.mobile"
        // 26 is the floor for the WebView APIs and the JRE packaging we need.
        // The W^X restriction that forces the JRE into jniLibs starts at 29,
        // but honouring it unconditionally is simpler than branching.
        minSdk = 26
        targetSdk = 35

        // Play refuses an upload whose versionCode it has already seen, so the
        // number has to be settable from CI:
        //   ./gradlew bundleRelease -PversionCode=2 -PversionName=0.1.1
        // The defaults are what a local build gets, and what the first store
        // upload will carry. A build that cannot bump its own version turns
        // every release into a source edit.
        val code = prop("versionCode", "1")
        versionCode = code.toIntOrNull()
            ?: throw GradleException("versionCode must be a whole number, got: $code")
        versionName = prop("versionName", "0.1.0")

        // Mirrors what the desktop app reads out of its package.json. Override
        // for a dev backend:  ./gradlew installDebug -PapiUrl=https://api.fractalnetworks.co
        buildConfigField("String", "API_URL", "\"${prop("apiUrl", "https://api.gethomerun.app")}\"")
        buildConfigField("String", "DISTRO_RELEASE_TAG", "\"${prop("distroReleaseTag", "")}\"")
        buildConfigField("String", "DEVICE_RELEASE_TAG", "\"${prop("deviceReleaseTag", "")}\"")
        buildConfigField("String", "GIT_COMMIT", "\"${prop("gitCommit", "")}\"")

        // The Ed25519 public key that over-the-air bundle manifests are signed
        // with. Public by nature — that is the point of signing asymmetrically
        // — so it is checked in rather than injected, and every build gets it
        // without anyone remembering a flag.
        //
        // That default matters. An empty key disables over-the-air updates
        // entirely (BundleUpdater refuses to fetch what it cannot verify, the
        // only safe reading of "no key configured"), and a release built
        // without the flag would look completely healthy while silently never
        // updating again. Hard to notice, and only noticeable months later.
        //
        // Generated 2026-08-13. Its private half lives in the
        // `ui-bundle-publish` environment's HOMERUN_BUNDLE_KEY secret and
        // nowhere else. **Changing this needs a store release** — a device only
        // accepts manifests signed by the key compiled into it, so every device
        // keeps the old key until it updates through the store.
        //
        // Override for testing against a throwaway key:
        //   ./gradlew assembleDebug -PbundlePublicKey=<64 hex chars>
        val bundlePublicKey = prop(
            "bundlePublicKey",
            "8d44ecfa010fe0136b450baee986a352cd027d3555403f0662dce5eb2ff16f4e",
        )
        // A typo here cannot be caught at runtime in any useful way: the app
        // would simply reject every manifest for ever, which is indistinguishable
        // from "no releases published". Fail the build instead.
        require(Regex("^[0-9a-f]{64}$").matches(bundlePublicKey)) {
            "bundlePublicKey must be 64 lowercase hex characters, got: $bundlePublicKey"
        }
        buildConfigField("String", "BUNDLE_PUBLIC_KEY", "\"$bundlePublicKey\"")

        // The staged Java runtime is architecture-specific and ~165 MB, so a
        // build ships exactly one ABI — the same choice Anvil-MC makes. Pass
        // the ABI that `npm run jre:*` staged:
        //   ./gradlew assembleRelease -Pabi=arm64-v8a
        // Omitted, every built ABI is packaged, which is right for local work
        // and wrong for a release — so a release without it fails, see
        // `verifyReleaseConfig`.
        requestedAbi?.let { abi ->
            ndk { abiFilters += abi }
        }
    }

    buildFeatures {
        buildConfig = true
    }

    // Release signing. The keystore and its passwords live in
    // `android/keystore.properties` — gitignored, supplied by CI from a secret,
    // never committed. Keys: storeFile, storePassword, keyAlias, keyPassword.
    //
    // With no such file the release build still completes, unsigned. That is
    // deliberate: conformance runs, CI smoke builds and local audit builds all
    // want a release APK and none of them can upload one, and requiring a
    // keystore to compile would put a copy of the signing key on every
    // developer's disk. What an unsigned artifact cannot do is reach Play — so
    // `verifyReleaseConfig` says so out loud rather than leaving the difference
    // to be noticed in the Play Console.
    val releaseKeystore = rootProject.file("keystore.properties").takeIf { it.exists() }?.let { file ->
        Properties().apply { file.inputStream().use { stream -> load(stream) } }
    }

    signingConfigs {
        releaseKeystore?.let { props ->
            create("release") {
                // A half-filled file would otherwise surface as a null
                // dereference somewhere inside the Android plugin.
                val missing = listOf("storeFile", "storePassword", "keyAlias", "keyPassword")
                    .filter { props.getProperty(it).isNullOrBlank() }
                if (missing.isNotEmpty()) {
                    throw GradleException(
                        "android/keystore.properties is missing: ${missing.joinToString(", ")}",
                    )
                }
                // Relative paths resolve against `android/`, so a keystore kept
                // beside the properties file needs only its name.
                storeFile = rootProject.file(props.getProperty("storeFile"))
                storePassword = props.getProperty("storePassword")
                keyAlias = props.getProperty("keyAlias")
                keyPassword = props.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        debug {
            applicationIdSuffix = ".debug"
            isMinifyEnabled = false
        }
        release {
            // Null when no keystore is configured, which leaves the artifact
            // unsigned rather than failing the build.
            signingConfig = signingConfigs.findByName("release")
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    androidResources {
        // The UI bundle is a static export of hashed, already-compressed
        // assets. Re-compressing them in the APK costs build time and buys
        // nothing.
        noCompress += listOf("woff2", "png", "webp", "jpg")

        // aapt's default asset filter includes `<dir>_*`, which drops every
        // directory whose name starts with an underscore. Next.js puts the
        // ENTIRE application bundle in `_next/`, so the default ships an APK
        // containing the HTML and none of the JavaScript — the app loads, the
        // scripts 404 into the SPA fallback, and the console fills with
        // "Unexpected token '<'". Everything below is the aapt default minus
        // that one pattern.
        ignoreAssetsPatterns.clear()
        ignoreAssetsPatterns += listOf(
            "!.svn", "!.git", "!.ds_store", "!*.scc", ".*",
            "!CVS", "!thumbs.db", "!picasa.ini", "!*~",
        )
    }

    packaging {
        // MUST stay true. The bundled JVM launcher ships as a jniLibs entry
        // (`libjavabin.so`) because API 29+ refuses to exec anything outside
        // nativeLibraryDir — and with legacy packaging off, nothing is
        // extracted there: the linker maps libraries straight from the APK and
        // no real file exists to exec. Flipping this to false costs install
        // size but makes hosting a Java server impossible.
        jniLibs.useLegacyPackaging = true
    }

    lint {
        // An unhandled bridge channel hangs a UI promise forever, so treat
        // the conformance gate as the real check and keep lint advisory.
        abortOnError = false
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity)
    implementation(libs.androidx.lifecycle.runtime)
    implementation(libs.androidx.webkit)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.kotlinx.serialization.json)
    implementation(libs.commons.compress)
    implementation(libs.tukaani.xz)
}

/**
 * The UI bundle is not committed — it is staged by `npm run ui:android` from
 * whatever the shared UI is currently at. Failing here with the fix spelled
 * out beats shipping an APK that shows a blank screen.
 */
val verifyUiBundle by tasks.registering {
    val indexHtml = layout.projectDirectory.file("src/main/assets/web/index.html")
    inputs.file(indexHtml).optional(true)
    doFirst {
        if (!indexHtml.asFile.exists()) {
            throw GradleException(
                "No UI bundle at app/src/main/assets/web/.\n" +
                    "Run `npm run ui:android` from the repo root first.",
            )
        }
    }
}

/**
 * The Java runtime is staged by `npm run jre:android`, not committed. Without
 * it the app builds and installs perfectly and then cannot host anything, so
 * say so at build time instead.
 *
 * And the runtime is architecture-specific, which is the sharper edge: staging
 * the x86_64 JRE for the emulator and then building `-Pabi=arm64-v8a` for a
 * phone produces an APK that installs, launches, shows every screen — and can
 * never start a server, because the only `java` in it is for the wrong CPU. The
 * ABI filter cannot catch that; the JRE lives in `assets/`, where nothing looks
 * at architecture. So read the architecture the runtime states about itself and
 * refuse the build when the two disagree.
 */
val verifyJavaRuntime by tasks.registering {
    val jre = layout.projectDirectory.dir("src/main/assets/jre")
    val marker = jre.file("java-major")
    val releaseFile = jre.file("release")
    inputs.file(marker).optional(true)
    inputs.file(releaseFile).optional(true)
    // Captured here, not read in the action: the action must not reach back
    // into the project while it runs.
    val abi = requestedAbi
    val expected = abi?.let { jreForAbi[it] }
    doFirst {
        if (!marker.asFile.exists()) {
            logger.warn(
                """
                WARNING: no Java runtime staged in app/src/main/assets/jre/.
                         This build cannot host a Java server. Stage one with:
                           npm run jre:android         (arm64, what ships)
                           npm run jre:android-x86_64  (emulator)
                """.trimIndent(),
            )
            return@doFirst
        }
        // No `-Pabi` means every ABI is packaged and there is nothing to
        // contradict; a release always passes one, see `verifyReleaseConfig`.
        if (expected == null) return@doFirst
        val (wantedArch, stageCommand) = expected

        // Every JDK image ships `release`, a shell-sourceable file of KEY="value".
        val staged = releaseFile.asFile.takeIf { it.exists() }?.readText()?.let {
            Regex("""^OS_ARCH="?([^"\n]+)"?$""", RegexOption.MULTILINE).find(it)
        }?.groupValues?.get(1)
            ?: throw GradleException(
                "The staged Java runtime does not say what architecture it is for " +
                    "(no OS_ARCH in app/src/main/assets/jre/release).\n" +
                    "Restage it:  $stageCommand",
            )

        if (staged != wantedArch) {
            throw GradleException(
                "The staged Java runtime is for the wrong architecture.\n" +
                    "  staged:    $staged  (app/src/main/assets/jre/release)\n" +
                    "  requested: $wantedArch  (-Pabi=$abi)\n" +
                    "This would build an APK that installs and can never host a " +
                    "server: the JRE lives in assets/, so nothing at install time " +
                    "notices the CPU is wrong.\n" +
                    "Restage the runtime:  $stageCommand",
            )
        }
    }
}

/**
 * The checks that only make sense for a release build.
 *
 * Both are about artifacts that look finished and are not: one that packages
 * every ABI and so carries a JRE for the wrong CPU alongside the right one, and
 * one that Play will not accept because nothing signed it.
 */
val verifyReleaseConfig by tasks.registering {
    val abi = requestedAbi
    val signed = android.buildTypes.getByName("release").signingConfig != null
    doFirst {
        if (abi == null) {
            throw GradleException(
                "A release build must name its ABI:  -Pabi=arm64-v8a\n" +
                    "Without it every built ABI is packaged, and only one of them " +
                    "matches the single Java runtime staged in assets/ — so the " +
                    "APK is both twice the size it should be and unable to host on " +
                    "the architectures it claims to support.",
            )
        }
        if (!signed) {
            logger.warn(
                """
                WARNING: no android/keystore.properties, so this release artifact
                         is UNSIGNED and cannot be uploaded to Play. Fine for a
                         local or CI build; not something to hand to the store.
                """.trimIndent(),
            )
        }
    }
}

/**
 * Every native binary a release has to carry, and the npm script that stages
 * it. The `-x86_64` suffix turns each into its emulator counterpart, which is
 * why all four are spelled as the arm64 name.
 *
 * Gradle has no idea `jniLibs` is generated, so a missing entry is not a build
 * failure — it is an APK that installs and then cannot do a thing the UI has
 * already told the player it can. `librestic.so` is the sharpest of the four:
 * `HostCapabilities.ANDROID` advertises `backups` unconditionally because the
 * constant describes the *platform* rather than the build, so without the
 * binary the app offers a feature that silently does nothing — and, the part
 * that costs data, a world played here never becomes the newest snapshot, so
 * the next desktop launch restores over it.
 */
val nativePayload = mapOf(
    "libhomerun_pumpkin_ffi.so" to "rust:android",
    "libjavabin.so" to "rust:java-launcher",
    "libwireproxy.so" to "wireproxy:android",
    "librestic.so" to "restic:android",
)

/**
 * The release's native payload is complete for the ABI it is being built for.
 *
 * Separate from [verifyJavaRuntime] because the failure is different in kind:
 * that one catches a runtime built for the wrong CPU, this one catches one
 * that was never staged at all. Both fail open without a check, and both
 * surface on a device a long way from the cause.
 */
val verifyNativePayload by tasks.registering {
    val abi = requestedAbi
    val libsDir = layout.projectDirectory.dir("src/main/jniLibs")
    doFirst {
        if (abi == null) return@doFirst // verifyReleaseConfig has already failed.
        val suffix = if (abi == "x86_64") "-x86_64" else ""
        val missing = nativePayload.filterKeys { !libsDir.dir(abi).file(it).asFile.exists() }
        if (missing.isNotEmpty()) {
            throw GradleException(
                "The $abi native payload is incomplete — this release would install " +
                    "and then fail on a device:\n" +
                    missing.entries.joinToString("\n") { (lib, script) ->
                        "    $lib   ->  npm run $script$suffix"
                    } +
                    "\n\n`npm run build:android:release` stages all of them.",
            )
        }
    }
}

tasks.matching { it.name.startsWith("merge") && it.name.endsWith("Assets") }
    .configureEach { dependsOn(verifyJavaRuntime) }

tasks.matching { it.name.startsWith("merge") && it.name.endsWith("ReleaseAssets") }
    .configureEach { dependsOn(verifyReleaseConfig, verifyNativePayload) }

tasks.matching { it.name.startsWith("merge") && it.name.endsWith("Assets") }
    .configureEach {
        dependsOn(verifyUiBundle)
        doLast {
            // The asset filter above is easy to lose in a merge and fails
            // silently — a blank app with a working build. Prove it held.
            val survived = outputs.files.files.any { File(it, "web/_next").isDirectory }
            if (!survived) {
                throw GradleException(
                    "The UI bundle's `_next/` directory did not survive asset merging.\n" +
                        "Check `androidResources.ignoreAssetsPatterns` — aapt's default " +
                        "`<dir>_*` pattern strips it, leaving an APK with no JavaScript.",
                )
            }
        }
    }
