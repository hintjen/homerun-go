package app.gethomerun.mobile

import android.content.Context
import android.os.Build
import android.util.Log
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
import org.apache.commons.compress.compressors.xz.XZCompressorInputStream
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

/**
 * Obtains a JVM, downloading it on demand — the same shape as the desktop's
 * Azul Zulu flow.
 *
 * # Why downloading is possible at all
 *
 * [Android 10](https://developer.android.com/about/versions/10/behavior-changes-10)
 * forbids `execve()` on anything in app storage, so a downloaded `java` binary
 * can never be run. It restricts `dlopen()` only for libraries with text
 * relocations, though, so an ordinary position-independent `.so` loads from
 * app storage fine.
 *
 * That gap is what [`homerun-java-launcher`][launcher] goes through: it ships
 * inside the APK as `libjavabin.so` (so it lives in `nativeLibraryDir`, the one
 * place exec is legal), and `dlopen`s the downloaded `libjvm.so`. Only that
 * 0.3 MB launcher is bundled; the runtime itself — tens of megabytes — is
 * fetched here.
 *
 * [launcher]: ../../../../../rust/homerun-java-launcher
 *
 * # Availability
 *
 * The published multiarch builds top out at **Java 17 for x86_64**; Java 21,
 * which Minecraft 1.20.5+ needs, is arm64-only. So an emulator can host up to
 * roughly 1.20.4 and newer versions need a physical device.
 */
object JavaRuntime {

    /** The launcher, renamed so the APK packager keeps it (`lib*.so` only). */
    private const val LAUNCHER = "libjavabin.so"

    /** Written after a successful unpack, so it happens once and not per launch. */
    private const val STAMP = ".complete"

    /**
     * Where each runtime comes from.
     *
     * Pinned by URL rather than resolved from a "latest" endpoint: a server
     * that worked yesterday must not break because an upstream tag moved.
     */
    data class Spec(val javaMajor: Int, val abi: String, val url: String)

    private val CATALOG = listOf(
        Spec(
            17, "arm64-v8a",
            "https://github.com/PojavLauncherTeam/android-openjdk-build-multiarch/releases/download/jre17-ca01427/jre17-arm64-20220817-release.tar.xz",
        ),
        Spec(
            17, "x86_64",
            "https://github.com/PojavLauncherTeam/android-openjdk-build-multiarch/releases/download/jre17-ec28559/jre17-x86_64-20210825-release.tar.xz",
        ),
    )

    private val abi: String get() = Build.SUPPORTED_ABIS.firstOrNull() ?: "unknown"

    /** The launcher, or null if this build ships none for the device's ABI. */
    fun launcher(context: Context): File? =
        File(context.applicationInfo.nativeLibraryDir, LAUNCHER).takeIf { it.canExecute() }

    fun isAvailable(context: Context): Boolean = launcher(context) != null

    /** Java versions this device could run, given what is published for its ABI. */
    fun supportedVersions(): List<Int> =
        CATALOG.filter { it.abi == abi }.map { it.javaMajor }.sorted()

    private fun home(context: Context, javaMajor: Int): File =
        File(context.filesDir, "runtimes/java$javaMajor")

    /** `libjvm.so` for an already-installed runtime, or null. */
    fun libjvm(context: Context, javaMajor: Int): File? =
        File(home(context, javaMajor), "lib/server/libjvm.so").takeIf { it.isFile }

    fun isInstalled(context: Context, javaMajor: Int): Boolean =
        File(home(context, javaMajor), STAMP).exists() && libjvm(context, javaMajor) != null

    /**
     * Ensure a runtime is present, downloading and unpacking if needed.
     *
     * Blocking — call it off the main thread. [onProgress] receives 0..1, or
     * -1 while the archive size is unknown.
     */
    fun ensure(
        context: Context,
        javaMajor: Int,
        onProgress: (Float) -> Unit = {},
    ): File {
        val target = home(context, javaMajor)
        if (isInstalled(context, javaMajor)) return target

        val spec = CATALOG.firstOrNull { it.javaMajor == javaMajor && it.abi == abi }
            ?: throw IllegalStateException(
                "No Java $javaMajor runtime is published for this device ($abi). " +
                    "Available here: ${supportedVersions().joinToString().ifEmpty { "none" }}."
            )

        // A partial unpack from an interrupted attempt must not look complete.
        target.deleteRecursively()
        target.mkdirs()

        Log.i(TAG, "downloading Java $javaMajor for $abi from ${spec.url}")
        try {
            download(spec.url, target, onProgress)
        } catch (err: Exception) {
            target.deleteRecursively()
            throw err
        }

        if (libjvm(context, javaMajor) == null) {
            target.deleteRecursively()
            throw IllegalStateException("The downloaded Java $javaMajor runtime has no libjvm.so.")
        }
        File(target, STAMP).writeText(spec.url)
        Log.i(TAG, "Java $javaMajor ready at $target")
        return target
    }

    /**
     * Streams the archive straight into place rather than staging it on disk
     * first — a phone should not need 40 MB free twice over.
     */
    private fun download(url: String, target: File, onProgress: (Float) -> Unit) {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            instanceFollowRedirects = true
            connectTimeout = 30_000
            readTimeout = 60_000
        }
        try {
            if (connection.responseCode !in 200..299) {
                throw IllegalStateException("Download failed with HTTP ${connection.responseCode}.")
            }
            val total = connection.contentLengthLong
            var read = 0L

            val counting = object : java.io.FilterInputStream(connection.inputStream) {
                override fun read(b: ByteArray, off: Int, len: Int): Int {
                    val n = super.read(b, off, len)
                    if (n > 0) {
                        read += n
                        onProgress(if (total > 0) read.toFloat() / total else -1f)
                    }
                    return n
                }
            }

            TarArchiveInputStream(XZCompressorInputStream(counting.buffered())).use { tar ->
                while (true) {
                    val entry = tar.nextEntry ?: break
                    val out = File(target, entry.name)
                    // Archive entries are untrusted input even from a source
                    // you trust; `..` would land files outside the target.
                    if (!out.canonicalPath.startsWith(target.canonicalPath + File.separator)) {
                        throw SecurityException("archive entry escapes the target: ${entry.name}")
                    }
                    when {
                        entry.isDirectory -> out.mkdirs()
                        // Symlinks are only used for the legal/ notices, and
                        // Android's filesystem rejects some of them. Skipping
                        // costs nothing; failing the whole install would not.
                        entry.isSymbolicLink -> Unit
                        else -> {
                            out.parentFile?.mkdirs()
                            out.outputStream().use(tar::copyTo)
                            // The JVM's own helper binaries and .so files need
                            // the execute bit; tar carries it, Java drops it.
                            if (entry.mode and 0b001_000_000 != 0) out.setExecutable(true, false)
                        }
                    }
                }
            }
        } finally {
            connection.disconnect()
        }
    }

    private const val TAG = "HomerunJava"
}
