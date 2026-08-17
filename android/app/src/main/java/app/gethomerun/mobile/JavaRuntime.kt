package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import java.io.File

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
     */
    fun available(context: Context): List<Int> = runCatching {
        (context.assets.list("") ?: emptyArray())
            .filter { it.startsWith(ASSET_PREFIX) }
            .mapNotNull { dir -> majorOf(context, dir) }
            .sorted()
    }.getOrDefault(emptyList())

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
        if (majorOf(context, assetDir) == null) {
            throw IllegalStateException(
                "This build ships no Java $major runtime. Stage one with " +
                    "`npm run jre:android` and rebuild."
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
                onProgress((index + 1).toFloat() / files.size)
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
