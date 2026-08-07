package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import java.io.File

/**
 * The bundled Java runtime.
 *
 * # Why it is bundled and not downloaded
 *
 * [Google Play's Device and Network Abuse policy](https://support.google.com/googleplay/android-developer/answer/16559646)
 * says an app "may not download executable code (such as dex, JAR, .so files)
 * from a source other than Google Play". Fetching `libjvm.so` at first run is
 * exactly that, and the carve-out does not rescue it: it covers code that runs
 * *in* a virtual machine, and `libjvm.so` **is** the virtual machine. So the
 * runtime ships inside the app, staged at build time by
 * `scripts/stage-jre.py`. Anvil-MC, which hosts Java servers on Play today,
 * does the same — see `docs/android-server-backend.md`.
 *
 * Server *jars* are a different matter: they are data the JVM reads, and they
 * are still downloaded.
 *
 * # Why assets, and not jniLibs
 *
 * `jniLibs` only packages files ending in `.so`, which would silently drop
 * `libz.so.1` — a versioned soname the linker asks for by name — and would
 * flatten a directory tree the JVM expects to walk from `java.home`. Assets
 * keep the layout intact, and the runtime is unpacked once into app storage.
 *
 * That is allowed: Android's W^X rule bans `exec` from app storage, not
 * `dlopen` of an ordinary position-independent library. Only the launcher
 * (`libjavabin.so`) is exec'd, and it alone lives in `nativeLibraryDir`.
 */
object JavaRuntime {

    /** The launcher, renamed so the APK packager keeps it (`lib*.so` only). */
    private const val LAUNCHER = "libjavabin.so"

    /** Asset directory that `scripts/stage-jre.py` writes. */
    private const val ASSET_ROOT = "jre"

    /**
     * Names the bundled version. Deliberately not dot-prefixed: aapt's asset
     * filter includes `.*`, so a hidden file never reaches the APK — the same
     * silent omission as the UI bundle's `_next/` directory.
     */
    private const val MARKER = "java-major"

    /** Written after a successful unpack, so it happens once and not per launch. */
    private const val STAMP = ".complete"

    /** Where the staged dependency libraries land, apart from the JRE's own. */
    const val DEPS_DIR = "termux-lib"

    /** The launcher, or null if this build ships none for the device's ABI. */
    fun launcher(context: Context): File? =
        File(context.applicationInfo.nativeLibraryDir, LAUNCHER).takeIf { it.canExecute() }

    /** True when this build has both a launcher and a staged runtime. */
    fun isAvailable(context: Context): Boolean =
        launcher(context) != null && javaMajor(context) != null

    /** Which Java version is bundled, or null if none was staged. */
    fun javaMajor(context: Context): Int? = runCatching {
        context.assets.open("$ASSET_ROOT/$MARKER").use {
            it.readBytes().decodeToString().trim().toInt()
        }
    }.getOrNull()

    fun home(context: Context): File = File(context.filesDir, "runtime")

    fun libjvm(context: Context): File? =
        File(home(context), "lib/server/libjvm.so").takeIf { it.isFile }

    fun isInstalled(context: Context): Boolean =
        File(home(context), STAMP).exists() && libjvm(context) != null

    /**
     * Unpack the bundled runtime if it has not been unpacked yet.
     *
     * Blocking and slow the first time — a hundred megabytes out of the APK —
     * so call it off the main thread. The bridge has no call timeout precisely
     * so the first server start is allowed to take as long as this needs.
     */
    fun ensure(context: Context, onProgress: (Float) -> Unit = {}): File {
        val target = home(context)
        if (isInstalled(context)) return target

        val version = javaMajor(context) ?: throw IllegalStateException(
            "This build ships no Java runtime. Stage one with " +
                "`npm run jre:android` and rebuild."
        )

        // A partial unpack from an interrupted attempt must not look complete.
        target.deleteRecursively()
        target.mkdirs()

        Log.i(TAG, "unpacking bundled Java $version")
        try {
            val files = list(context, ASSET_ROOT)
            files.forEachIndexed { index, path ->
                copy(context, path, File(target, path.removePrefix("$ASSET_ROOT/")))
                onProgress((index + 1).toFloat() / files.size)
            }
        } catch (err: Exception) {
            target.deleteRecursively()
            throw err
        }

        if (libjvm(context) == null) {
            target.deleteRecursively()
            throw IllegalStateException("The bundled runtime has no libjvm.so.")
        }
        File(target, STAMP).writeText(version.toString())
        Log.i(TAG, "Java $version ready at $target")
        return target
    }

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
