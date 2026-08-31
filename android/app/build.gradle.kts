// Imported rather than written out: inside this script `java` is the Java
// plugin extension, so `java.util.Properties` does not resolve.
import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.google.services)
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
 * Whether this build has anything to do with over-the-air UI bundles.
 *
 * On by default, including for debug builds — the update path is only ever
 * exercised on a debug build, so defaulting it off in development would mean
 * nobody sees it work until a release. Turn it off for a build whose whole
 * point is the UI you just staged into `assets/web/`:
 *
 *   ./gradlew installDebug -PotaUpdates=off
 *   npm run android:run -- --no-ota
 *
 * Off means *ignore them entirely*, not merely "do not fetch": nothing is
 * downloaded, and a bundle already sitting in `files/ui/` is neither promoted
 * nor served. Nothing on disk is deleted either, so a build with the flag back
 * on picks up exactly where it left off.
 *
 * A **release** built this way would look completely healthy while silently
 * never updating again, which is the same failure an empty signing key would
 * cause — so `verifyReleaseConfig` refuses one.
 */
val otaUpdates: Boolean = when (prop("otaUpdates", "on").lowercase()) {
    "on", "true", "yes", "1" -> true
    "off", "false", "no", "0" -> false
    else -> throw GradleException(
        "otaUpdates must be on or off, got: ${project.findProperty("otaUpdates")}",
    )
}

/**
 * Per ABI: the `OS_ARCH` a Java runtime reports in its `release` file, and the
 * command that stages one. Both belong to `scripts/stage-jre.py`, the only
 * thing that ever writes into `assets/jre-*`; it takes the same two ABI names.
 */
val jreForAbi = mapOf(
    "arm64-v8a" to Pair("aarch64", "npm run jre:android"),
    "x86_64" to Pair("x86_64", "npm run jre:android-x86_64"),
)

/**
 * The Java runtimes a **release** has to carry, as `assets/jre-<major>`.
 *
 * 25 runs the current Minecraft release; 21 is what the mod loaders want, and
 * running them on 25 is a failure rather than an upgrade. `homerun-core`'s
 * `jar::select_runtime` picks between them per server and can only pick from
 * what shipped — so a release missing one silently loses every server that
 * needed it. Debug builds may stage whichever subset is convenient.
 *
 * Kept in step with `DEFAULT_JAVA` in `scripts/stage-jre.py`.
 */
val releaseJavaRuntimes = listOf(21, 25)

/**
 * Of those, the ones Play delivers on demand instead of packaging in the base
 * APK — one feature module per major, `:jre<major>`.
 *
 * Both of them. Two runtimes at ~54 and ~59 MB compressed put the install at
 * ~167 MB of Play's 200 MB ceiling; deferring both drops it to ~54 MB. Neither
 * is wanted at all by a device that only hosts Pumpkin, which needs no JVM, and
 * a device that does host Java pays for the runtime its servers actually
 * select rather than for both.
 *
 * The cost is that no Java server can start until Play has delivered one, so a
 * delivery that fails is a launch that fails. [JavaRuntime.fetchModule] is
 * where that surfaces, in a player's words rather than a log's.
 *
 * This list is the *promise*, not the delivery. [JavaRuntime.available] reports
 * these majors as available before the module is on the device, because the
 * core chooses a runtime from that list and a jar needing 21 must still be able
 * to ask for it — the download happens inside `ensure`. Ship a build that
 * omits a major here and the core simply never picks it.
 */
val onDemandJavaRuntimes = listOf(21, 25)

/**
 * Every place a staged runtime can live, in the order they are searched.
 *
 * The build checks below predate the feature module and looked only in
 * `app/src/main/assets`; left that way they would call Java 21 missing on
 * every release that correctly staged it.
 */
val javaRuntimeAssetRoots: List<File> = listOf(
    layout.projectDirectory.dir("src/main/assets").asFile,
) + onDemandJavaRuntimes.map { rootProject.file("jre$it/src/main/assets") }

android {
    namespace = "app.gethomerun.mobile"
    compileSdk = 36

    defaultConfig {
        applicationId = "app.gethomerun.mobile"
        // 26 is the floor for the WebView APIs and the JRE packaging we need.
        // The W^X restriction that forces the JRE into jniLibs starts at 29,
        // but honouring it unconditionally is simpler than branching.
        minSdk = 26
        targetSdk = 36

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

        // Note this is *not* spelled as an empty key. `BundleUpdater` treats a
        // blank key as "off" and always has, but `prop` falls back to the
        // default for a blank override, so there was no way to reach that state
        // from a command line — and the regex above rejects it besides. This is
        // the switch; the key stays a key.
        buildConfigField("boolean", "OTA_UPDATES", otaUpdates.toString())

        // Which majors arrive as feature modules rather than in the APK.
        // [JavaRuntime] cannot discover these by listing assets — that is the
        // whole point of them — so the build states it.
        buildConfigField(
            "String",
            "ON_DEMAND_JAVA",
            "\"${onDemandJavaRuntimes.joinToString(",")}\"",
        )

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

    // One module per on-demand runtime. Their assets are staged by
    // `npm run jre:android` exactly as the base APK's are.
    dynamicFeatures += onDemandJavaRuntimes.map { ":jre$it" }.toSet()

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
    implementation(libs.firebase.messaging)
    implementation(libs.play.feature.delivery)
}

/**
 * Stage the google-services.json that matches the backend this build talks to.
 *
 * The pairing that matters is app Firebase project <-> backend FCM credential,
 * NOT debug-vs-release: a debug build against the production API registers its
 * token with the production backend, and if that token was minted in the
 * staging Firebase project every send dies as SENDER_ID_MISMATCH. So the file
 * follows `-PapiUrl` exactly the way the API URL itself does. The two source
 * files sit gitignored in the repo root (see docs/building.md, "Push
 * credentials"); missing means push silently cannot work, so fail the build
 * with the fix spelled out instead.
 */
val stageGoogleServices by tasks.registering {
    val apiUrl = prop("apiUrl", "https://api.gethomerun.app")
    val flavor = if (apiUrl.contains("gethomerun.app")) "prod" else "staging"
    val source = rootProject.layout.projectDirectory.file("../$flavor-android-google-services.json")
    val target = layout.projectDirectory.file("google-services.json")
    inputs.file(source).optional(true)
    outputs.file(target)
    doFirst {
        if (!source.asFile.exists()) {
            throw GradleException(
                "No $flavor-android-google-services.json in the repo root.\n" +
                    "Download it from the Firebase console ($flavor project) — " +
                    "see docs/building.md, \"Push credentials\".",
            )
        }
        source.asFile.copyTo(target.asFile, overwrite = true)
    }
}

// The google-services plugin reads app/google-services.json during resource
// processing; make sure the staged copy is in place first.
tasks.matching { it.name.startsWith("process") && it.name.endsWith("GoogleServices") }
    .configureEach { dependsOn(stageGoogleServices) }

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
    inputs.files(javaRuntimeAssetRoots)
    // Captured here, not read in the action: the action must not reach back
    // into the project while it runs.
    val abi = requestedAbi
    val expected = abi?.let { jreForAbi[it] }
    val roots = javaRuntimeAssetRoots
    val rootDir = rootProject.projectDir
    doFirst {
        val staged = roots
            .flatMap { (it.listFiles { f: File -> f.isDirectory } ?: emptyArray()).toList() }
            .filter { it.name.startsWith("jre-") && File(it, "java-major").isFile }
            .sortedBy { it.name }

        if (staged.isEmpty()) {
            logger.warn(
                """
                WARNING: no Java runtime staged in any of jre-*/.
                         This build cannot host a Java server. Stage them with:
                           npm run jre:android         (arm64, what ships)
                           npm run jre:android-x86_64  (emulator)
                """.trimIndent(),
            )
            return@doFirst
        }
        logger.lifecycle(
            "Java runtimes staged: ${staged.joinToString(", ") { it.name.removePrefix("jre-") }}",
        )

        // No `-Pabi` means every ABI is packaged and there is nothing to
        // contradict; a release always passes one, see `verifyReleaseConfig`.
        if (expected == null) return@doFirst
        val (wantedArch, stageCommand) = expected

        // Every runtime is unpacked and dlopen'd on its own, so every runtime
        // has to be right on its own. One staged for the wrong CPU alongside a
        // correct one is the worse failure of the two: the app hosts fine until
        // someone picks the Minecraft version that selects the broken runtime.
        for (runtime in staged) {
            // Every JDK image ships `release`, a shell-sourceable file of KEY="value".
            val arch = File(runtime, "release").takeIf { it.isFile }?.readText()?.let {
                Regex("""^OS_ARCH="?([^"\n]+)"?$""", RegexOption.MULTILINE).find(it)
            }?.groupValues?.get(1)
                ?: throw GradleException(
                    "The staged Java runtime in ${runtime.relativeTo(rootDir)} does not say what " +
                        "architecture it is for (no OS_ARCH in its `release` file).\n" +
                        "Restage it:  $stageCommand",
                )

            if (arch != wantedArch) {
                throw GradleException(
                    "A staged Java runtime is for the wrong architecture.\n" +
                        "  staged:    $arch  (${runtime.relativeTo(rootDir)}/release)\n" +
                        "  requested: $wantedArch  (-Pabi=$abi)\n" +
                        "This would build an APK that installs and can never host a " +
                        "server on that runtime: the JRE lives in assets/, so nothing " +
                        "at install time notices the CPU is wrong.\n" +
                        "Restage the runtimes:  $stageCommand",
                )
            }
        }
    }
}

/**
 * A release carries every runtime [releaseJavaRuntimes] names.
 *
 * Separate from [verifyJavaRuntime], which is about a runtime being *wrong*;
 * this one is about a runtime being *absent*. The failure is quiet and remote:
 * the app installs, hosts most servers perfectly, and refuses exactly the ones
 * whose Minecraft version selects the runtime that never shipped.
 */
val verifyReleaseRuntimes by tasks.registering {
    inputs.files(javaRuntimeAssetRoots)
    val wanted = releaseJavaRuntimes
    val roots = javaRuntimeAssetRoots
    doFirst {
        val missing = wanted.filter { major ->
            roots.none { File(it, "jre-$major/java-major").isFile }
        }
        if (missing.isNotEmpty()) {
            throw GradleException(
                "This release is missing the Java ${missing.joinToString(" and ")} " +
                    "runtime${if (missing.size > 1) "s" else ""}.\n" +
                    "`homerun-core` chooses a runtime per server and can only choose from " +
                    "what shipped, so every server needing " +
                    "${if (missing.size > 1) "one of these" else "this one"} would be " +
                    "refused on a device.\n" +
                    "Stage them all:  npm run jre:android",
            )
        }
    }
}

/**
 * The checks that only make sense for a release build.
 *
 * All three are about artifacts that look finished and are not: one that
 * packages every ABI and so carries a JRE for the wrong CPU alongside the right
 * one, one that Play will not accept because nothing signed it, and one that
 * can never be fixed without another store release.
 */
val verifyReleaseConfig by tasks.registering {
    val abi = requestedAbi
    val signed = android.buildTypes.getByName("release").signingConfig != null
    val ota = otaUpdates
    doFirst {
        if (!ota) {
            throw GradleException(
                "This release was built with -PotaUpdates=off, so it would never " +
                    "take a UI bundle over the air.\n" +
                    "Nothing about it would look wrong: it installs, runs, and " +
                    "silently stays on the UI compiled into it for ever — every " +
                    "shared-UI fix would need another store release.\n" +
                    "The flag is for development builds. Drop it.",
            )
        }
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
    // The Pumpkin server itself. Android links no engine into the `.so` any
    // more — it runs one as a child process — so a build without this stages
    // a host that advertises Pumpkin and cannot start it.
    "libpumpkin.so" to "rust:pumpkin-bin",
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
    .configureEach { dependsOn(verifyReleaseConfig, verifyNativePayload, verifyReleaseRuntimes) }

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
