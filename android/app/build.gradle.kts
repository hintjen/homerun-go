plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.serialization)
}

/** A `-P` override, or the default. Keeps build config out of source. */
fun prop(name: String, fallback: String): String =
    (project.findProperty(name) as String?)?.takeIf { it.isNotBlank() } ?: fallback

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
        versionCode = 1
        versionName = "0.1.0"

        // Mirrors what the desktop app reads out of its package.json. Override
        // for a dev backend:  ./gradlew installDebug -PapiUrl=https://api.fractalnetworks.co
        buildConfigField("String", "API_URL", "\"${prop("apiUrl", "https://api.gethomerun.app")}\"")
        buildConfigField("String", "DISTRO_RELEASE_TAG", "\"${prop("distroReleaseTag", "")}\"")
        buildConfigField("String", "DEVICE_RELEASE_TAG", "\"${prop("deviceReleaseTag", "")}\"")
        buildConfigField("String", "GIT_COMMIT", "\"${prop("gitCommit", "")}\"")
    }

    buildFeatures {
        buildConfig = true
    }

    buildTypes {
        debug {
            applicationIdSuffix = ".debug"
            isMinifyEnabled = false
        }
        release {
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
