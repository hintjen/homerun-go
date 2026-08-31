package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import com.google.android.play.core.splitcompat.SplitCompat
import com.google.android.play.core.splitinstall.SplitInstallException
import com.google.android.play.core.splitinstall.SplitInstallManagerFactory
import com.google.android.play.core.splitinstall.SplitInstallRequest
import com.google.android.play.core.splitinstall.SplitInstallStateUpdatedListener
import com.google.android.play.core.splitinstall.model.SplitInstallErrorCode
import com.google.android.play.core.splitinstall.model.SplitInstallSessionStatus
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicReference

/**
 * The bundled Java runtimes.
 *
 * # Why they are bundled and not downloaded
 *
 * [Google Play's Device and Network Abuse policy](https://support.google.com/googleplay/android-developer/answer/16559646)
 * says an app "may not download executable code (such as dex, JAR, .so files)
 * from a source other than Google Play". Fetching `libjvm.so` at first run is
 * exactly that, and the carve-out does not rescue it: it covers code that runs
 * *in* a virtual machine, and `libjvm.so` **is** the virtual machine. So the
 * runtimes ship inside the app, staged at build time by
 * `scripts/stage-jre.py`. Anvil-MC, which hosts Java servers on Play today,
 * does the same — see `docs/android-server-backend.md`.
 *
 * Server *jars* are a different matter: they are data the JVM reads, and they
 * are still downloaded.
 *
 * # Why one of them arrives later anyway
 *
 * Both runtimes in the APK put the install at ~167 MB of Play's 200 MB
 * ceiling. Java 21 is therefore delivered by **Play Feature Delivery** — an
 * on-demand feature module, `:jre21` — which is not counted against that
 * ceiling and is still Play doing the delivering, so the policy above is
 * satisfied exactly as it is for the runtime in the APK.
 *
 * It is a feature module and not an *asset pack* for the same policy reason
 * read the other way: asset packs are documented as carrying "no executable
 * code", and a JRE is `libjvm.so`. Feature Delivery is the sanctioned route
 * for code.
 *
 * What that costs this file is one thing: a runtime can be *promised* and not
 * yet *present*. [available] reports what the build can provide, because the
 * core chooses from that list and a jar needing 17 has to be able to ask for
 * it; [ensure] is where the module is actually fetched. Everything after the
 * fetch is unchanged — SplitCompat merges the split into the same
 * `AssetManager`, so the unpack below cannot tell the two apart.
 *
 * **Being a module is not the same as being downloaded.** All three majors live
 * in modules; their manifests choose when each arrives. 25 is packaged with the
 * app, 17 never is, and 21 is install-time until the listing clears review.
 * This file does not need to know which: it fetches when the assets are not
 * there and does nothing when they are, so an install-time module takes the
 * same path as one already downloaded.
 *
 * # Why there is more than one
 *
 * Minecraft needs a Java at least as new as the version it names, and the mod
 * loaders coming in `plans/android-mod-loaders.md` need one that is *exactly*
 * right. This build stages Java 21 and 25; `homerun-core` picks between them
 * per server ([Core.selectRuntime]) and this file does as it is told.
 *
 * # Why assets, and not jniLibs
 *
 * `jniLibs` only packages files ending in `.so`, which would silently drop
 * `libz.so.1` — a versioned soname the linker asks for by name — and would
 * flatten a directory tree the JVM expects to walk from `java.home`. Assets
 * keep the layout intact, and a runtime is unpacked once into app storage.
 *
 * That is allowed: Android's W^X rule bans `exec` from app storage, not
 * `dlopen` of an ordinary position-independent library. Only the launcher
 * (`libjavabin.so`) is exec'd, and it alone lives in `nativeLibraryDir`.
 *
 * # Why unpacking is lazy
 *
 * A runtime is ~170 MB out of the APK and the first unpack is already the
 * slowest thing a first launch does. Two runtimes must not mean two unpacks:
 * [ensure] takes the major it is asked for and touches nothing else, so a
 * player who only ever hosts vanilla never pays for the one they do not use.
 */
object JavaRuntime {

    /** The launcher, renamed so the APK packager keeps it (`lib*.so` only). */
    private const val LAUNCHER = "libjavabin.so"

    /**
     * One staged runtime per asset directory, named for its Java major.
     *
     * `scripts/stage-jre.py` writes these and nothing else does. The prefix is
     * the contract between the two: this file discovers runtimes by listing
     * the asset root and matching it.
     */
    private const val ASSET_PREFIX = "jre-"

    /**
     * Names the version inside each runtime directory. Deliberately not
     * dot-prefixed: aapt's asset filter includes `.*`, so a hidden file never
     * reaches the APK — the same silent omission as the UI bundle's `_next/`
     * directory.
     */
    private const val MARKER = "java-major"

    /** Written after a successful unpack, so it happens once and not per launch. */
    private const val STAMP = ".complete"

    /** A feature module is named for the major it carries: `jre21`. */
    private const val MODULE_PREFIX = "jre"

    /**
     * How much of [ensure]'s progress the module download is worth.
     *
     * A first run that has to fetch does two slow things in a row, and a bar
     * that reached the end and restarted would read as a stall. The download
     * gets the larger share because it usually is: ~54 MB over a phone's
     * network against an unpack from local storage.
     */
    private const val DOWNLOAD_SHARE = 0.6f

    /**
     * Request code for Play's download confirmation.
     *
     * Nothing reads the result. The session reports what the player chose
     * through the same state listener the download uses, so the answer arrives
     * as DOWNLOADING or CANCELED rather than as an activity result — this only
     * has to not collide with anything else the activity starts.
     */
    private const val CONFIRM_REQUEST = 8021

    /** Where the staged dependency libraries land, apart from the JRE's own. */
    const val DEPS_DIR = "termux-lib"

    /** The launcher, or null if this build ships none for the device's ABI. */
    fun launcher(context: Context): File? =
        File(context.applicationInfo.nativeLibraryDir, LAUNCHER).takeIf { it.canExecute() }

    /**
     * Every Java major this build staged, ascending.
     *
     * Read from the APK rather than from a constant, so a build that staged
     * only one runtime — which `npm run jre:android --java 25` will do, and
     * which every debug build is free to do — describes itself honestly
     * instead of promising a runtime it does not carry.
     *
     * A runtime that has not been downloaded yet still counts as available:
     * [Core.selectRuntime] picks from this list, so omitting 17 until it
     * downloads would mean nothing ever selects it and it never downloads.
     * [ensure] fetches it at the point of use.
     */
    fun available(context: Context): List<Int> = runCatching {
        val staged = (context.assets.list("") ?: emptyArray())
            .filter { it.startsWith(ASSET_PREFIX) }
            .mapNotNull { dir -> majorOf(context, dir) }
        (staged + deliverable(context)).distinct().sorted()
    }.getOrDefault(emptyList())

    /**
     * The on-demand majors this build can still honestly promise.
     *
     * Present in the split already, or not installed at all — in which case
     * Play can deliver it on request. The case this excludes is the debug
     * build that installed an empty module because `npm run jre:android-25`
     * staged only 25: the module is there, carries no runtime, and Play has
     * nothing further to send. Claiming 21 there would promise a runtime that
     * can never arrive, which is exactly what this function exists to avoid.
     */
    private fun deliverable(context: Context): List<Int> = modules().filter { major ->
        majorOf(context, "$ASSET_PREFIX$major") != null || !isModuleInstalled(context, major)
    }

    /**
     * The majors the build wired as feature modules. Empty is a valid answer.
     *
     * Says nothing about *when* each arrives — an install-time module is in
     * here too, and simply has its assets present already.
     */
    private fun modules(): List<Int> =
        BuildConfig.MODULE_JAVA.split(",").mapNotNull { it.trim().toIntOrNull() }

    private fun isModuleInstalled(context: Context, major: Int): Boolean = runCatching {
        SplitInstallManagerFactory.create(context).installedModules.contains(module(major))
    }.getOrDefault(false)

    private fun module(major: Int) = "$MODULE_PREFIX$major"

    /** True when this build has a launcher and at least one staged runtime. */
    fun isAvailable(context: Context): Boolean =
        launcher(context) != null && available(context).isNotEmpty()

    fun home(context: Context, major: Int): File = File(context.filesDir, "runtime-$major")

    fun libjvm(context: Context, major: Int): File? =
        File(home(context, major), "lib/server/libjvm.so").takeIf { it.isFile }

    fun isInstalled(context: Context, major: Int): Boolean =
        File(home(context, major), STAMP).exists() && libjvm(context, major) != null

    /**
     * Unpack the runtime for [major] if it has not been unpacked yet.
     *
     * Blocking and slow the first time — a hundred and seventy megabytes out
     * of the APK — so call it off the main thread. The bridge has no call
     * timeout precisely so the first server start is allowed to take as long
     * as this needs.
     *
     * Only [major] is touched. The other staged runtime stays in the APK,
     * costing storage but not time, until something asks for it.
     */
    fun ensure(context: Context, major: Int, onProgress: (Float) -> Unit = {}): File {
        val target = home(context, major)
        if (isInstalled(context, major)) return target

        val assetDir = "$ASSET_PREFIX$major"

        // A delivered runtime is not in the APK until Play has sent the
        // module. Blocking is correct here: the caller is already on an IO
        // thread inside a launch that deliberately has no call timeout.
        var floor = 0f
        if (major in modules() && majorOf(context, assetDir) == null) {
            fetchModule(context, major) { onProgress(it * DOWNLOAD_SHARE) }
            floor = DOWNLOAD_SHARE
        }

        if (majorOf(context, assetDir) == null) {
            throw IllegalStateException(
                if (major in modules()) {
                    // The module is installed and carries nothing, which Play
                    // cannot fix by sending it again: a debug build staged a
                    // subset. [available] filters this case out, so reaching
                    // here means something asked for a major it was not
                    // offered. `npm run jre:android` stages all three.
                    "The Java $major module carries no runtime. Stage it with " +
                        "`npm run jre:android` and rebuild."
                } else {
                    "This build ships no Java $major runtime. Stage one with " +
                        "`npm run jre:android` and rebuild."
                }
            )
        }

        // A partial unpack from an interrupted attempt must not look complete.
        target.deleteRecursively()
        target.mkdirs()

        Log.i(TAG, "unpacking bundled Java $major")
        try {
            val files = list(context, assetDir)
            files.forEachIndexed { index, path ->
                copy(context, path, File(target, path.removePrefix("$assetDir/")))
                onProgress(floor + (1f - floor) * (index + 1).toFloat() / files.size)
            }
        } catch (err: Exception) {
            target.deleteRecursively()
            throw err
        }

        if (libjvm(context, major) == null) {
            target.deleteRecursively()
            throw IllegalStateException("The staged Java $major runtime has no libjvm.so.")
        }
        File(target, STAMP).writeText(major.toString())
        Log.i(TAG, "Java $major ready at $target")
        return target
    }

    /**
     * Drop unpacked runtimes this build no longer ships.
     *
     * An app updated from a build that staged Java 17 leaves ~500 MB of
     * unpacked runtime in `filesDir` that nothing will ever launch again, and
     * nothing else would ever collect it — the unpack is keyed by major, so a
     * version that stopped being staged simply stops being asked for.
     *
     * Safe to call at any time: a runtime in use is one this build still
     * ships, so it is never in the removal set.
     */
    fun dropUnusedRuntimes(context: Context) {
        val keep = available(context).map { "runtime-$it" }.toSet()
        val dirs = context.filesDir.listFiles { f -> f.isDirectory } ?: return
        for (dir in dirs) {
            if (!dir.name.startsWith("runtime-") || dir.name in keep) continue
            Log.i(TAG, "dropping unpacked ${dir.name}; this build no longer ships it")
            dir.deleteRecursively()
        }
    }

    /**
     * Ask Play for one on-demand runtime, and block until it is here.
     *
     * Returns immediately for a sideloaded debug build: Gradle installs
     * feature modules as splits beside the app, so `installedModules` already
     * names it and no Play Store is involved. That is what keeps
     * `npm run android:run` working on a device that has never seen this
     * build on Play.
     *
     * States are matched on the module name rather than the session id. The id
     * is only known once `startInstall` succeeds, and an install served from
     * cache can reach INSTALLED before that — filtering on the id would drop
     * the event that ends the wait and hang the launch for good.
     *
     * The wait itself is unbounded, deliberately: a runtime is ~54 MB and a
     * player on a slow connection is still making progress. What ends it is
     * always a terminal state from Play — including the one for a confirmation
     * the player never answered, which Play expires on its own.
     */
    private fun fetchModule(context: Context, major: Int, onProgress: (Float) -> Unit) {
        val name = module(major)
        val manager = SplitInstallManagerFactory.create(context)

        if (name in manager.installedModules) {
            // Installed, but this process may not have been told. SplitCompat
            // is what makes the split's assets visible to the AssetManager.
            SplitCompat.install(context)
            return
        }

        Log.i(TAG, "asking Play for the Java $major runtime module")
        val done = CountDownLatch(1)
        val failure = AtomicReference<String?>(null)

        val listener = SplitInstallStateUpdatedListener { state ->
            if (!state.moduleNames().contains(name)) return@SplitInstallStateUpdatedListener
            when (state.status()) {
                SplitInstallSessionStatus.DOWNLOADING -> {
                    val total = state.totalBytesToDownload()
                    if (total > 0) onProgress(state.bytesDownloaded().toFloat() / total)
                }
                SplitInstallSessionStatus.INSTALLED -> done.countDown()
                SplitInstallSessionStatus.FAILED -> {
                    failure.set(refusal(major, state.errorCode()))
                    done.countDown()
                }
                SplitInstallSessionStatus.CANCELED -> {
                    failure.set(
                        "The Java $major runtime download was cancelled. Start the " +
                            "server again to retry."
                    )
                    done.countDown()
                }
                // Play asks before downloading over a metered connection, and
                // the prompt has to be raised on an activity.
                //
                // Showing it settles nothing here. Play carries the session on
                // afterwards, so this same listener sees DOWNLOADING and then
                // INSTALLED, or CANCELED if the player declined — the wait ends
                // on one of those, not on an activity result.
                SplitInstallSessionStatus.REQUIRES_USER_CONFIRMATION -> {
                    val activity = ForegroundActivity.get()
                    val asked = activity != null && runCatching {
                        manager.startConfirmationDialogForResult(state, activity, CONFIRM_REQUEST)
                    }.getOrElse {
                        Log.w(TAG, "could not raise Play's confirmation: ${it.message}")
                        false
                    }
                    if (!asked) {
                        // The app is in the background, which a launch is
                        // entitled to be — the foreground service exists so a
                        // server can start without a screen. There is nothing
                        // to ask on, so say what would let it through instead
                        // of waiting for an answer nobody was asked for.
                        Log.w(TAG, "Java $major needs confirmation and there is no activity to ask on")
                        failure.set(
                            "Downloading the Java $major runtime needs your confirmation. " +
                                "Open Homerun Go, or connect to Wi-Fi, and start the server again."
                        )
                        done.countDown()
                    }
                }
                else -> Unit
            }
        }

        manager.registerListener(listener)
        try {
            val request = SplitInstallRequest.newBuilder().addModule(name).build()
            manager.startInstall(request).addOnFailureListener { err ->
                val code = (err as? SplitInstallException)?.errorCode
                Log.w(TAG, "Play refused the Java $major module: ${code ?: err.message}")
                failure.set(
                    if (code != null) {
                        refusal(major, code)
                    } else {
                        "Google Play could not send the Java $major runtime " +
                            "(${err.message}). Start the server again, and check your " +
                            "connection if it keeps failing."
                    }
                )
                done.countDown()
            }
            done.await()
        } finally {
            manager.unregisterListener(listener)
        }

        failure.get()?.let { throw IllegalStateException(it) }

        // Only now are the split's assets readable in this process.
        SplitCompat.install(context)
        Log.i(TAG, "Play delivered the Java $major runtime module")
    }

    /**
     * What to tell a player when Play refuses to send a runtime.
     *
     * These codes differ in what the player can actually do, and the single
     * "check your connection" that used to cover all of them is wrong for most:
     * an app Play does not consider owned, and a phone with no room on it, are
     * not network problems. Advising a retry on Wi-Fi for either sends someone
     * round a loop that cannot terminate.
     *
     * [SplitInstallErrorCode.APP_NOT_OWNED] is the one to recognise while
     * testing, and it is not what it sounds like. A legitimate internal
     * testing track install returns it too, while the app's Play listing is
     * unreviewed and its setup unfinished — Play has no acquisition record to
     * check against, so it refuses every module and no Java server can start.
     * Nothing on the device distinguishes that from a sideload either;
     * `installerPackageName` says `com.android.vending` regardless. See
     * `docs/android-server-backend.md` for what was ruled out getting there.
     */
    private fun refusal(major: Int, code: Int): String = when (code) {
        SplitInstallErrorCode.APP_NOT_OWNED ->
            "This copy of Homerun Go did not come from Google Play, so Play will not " +
                "send it the Java $major runtime. Install Homerun Go from the Play " +
                "Store to host a Java server."

        SplitInstallErrorCode.INSUFFICIENT_STORAGE ->
            "There is not enough room on this phone for the Java $major runtime, " +
                "which needs about 170 MB. Free some space and start the server again."

        SplitInstallErrorCode.NETWORK_ERROR ->
            "The Java $major runtime could not be downloaded. Check your connection " +
                "and start the server again."

        SplitInstallErrorCode.MODULE_UNAVAILABLE ->
            "This version of Homerun Go cannot get the Java $major runtime from " +
                "Google Play. Update the app, then start the server again."

        SplitInstallErrorCode.PLAY_STORE_NOT_FOUND, SplitInstallErrorCode.API_NOT_AVAILABLE ->
            "Google Play on this phone cannot send the Java $major runtime, and " +
                "Homerun Go needs it to host a Java server."

        SplitInstallErrorCode.ACCESS_DENIED ->
            "Android would not let Homerun Go download in the background. Open " +
                "Homerun Go and start the server again."

        else ->
            "Google Play could not send the Java $major runtime (error $code). Start " +
                "the server again, and check your connection if it keeps failing."
    }

    /** The major a staged asset directory declares, or null if it is not one. */
    private fun majorOf(context: Context, assetDir: String): Int? = runCatching {
        context.assets.open("$assetDir/$MARKER").use {
            it.readBytes().decodeToString().trim().toInt()
        }
    }.getOrNull()

    /**
     * Every asset path under [dir], depth-first.
     *
     * `AssetManager.list` cannot tell a file from a directory, so an empty
     * listing is taken to mean "file" — true for this tree, where no directory
     * is empty.
     */
    private fun list(context: Context, dir: String): List<String> {
        val children = context.assets.list(dir) ?: return emptyList()
        if (children.isEmpty()) return listOf(dir)
        return children.flatMap { list(context, "$dir/$it") }
    }

    private fun copy(context: Context, assetPath: String, out: File) {
        out.parentFile?.mkdirs()
        context.assets.open(assetPath).use { input ->
            out.outputStream().use { input.copyTo(it) }
        }
        // Assets carry no permissions, and the runtime's libraries and helper
        // binaries need the execute bit back.
        if (out.name.contains(".so") || out.parentFile?.name == "bin") {
            out.setExecutable(true, false)
        }
    }

    private const val TAG = "HomerunJava"
}
