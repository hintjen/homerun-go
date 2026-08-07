package app.gethomerun.mobile

import android.content.Context
import android.os.Build
import android.util.Log
import org.apache.commons.compress.archivers.ar.ArArchiveInputStream
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
import org.apache.commons.compress.compressors.xz.XZCompressorInputStream
import java.io.File
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.nio.file.Files

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
 * That gap is what `rust/homerun-java-launcher` goes through: it ships inside
 * the APK as `libjavabin.so` (the one place exec is legal) and `dlopen`s the
 * downloaded `libjvm.so`. Only that 0.3 MB launcher is bundled; the runtime —
 * a hundred megabytes — is fetched here.
 *
 * # Where runtimes come from
 *
 * [Termux](https://packages.termux.dev/apt/termux-main/), which publishes
 * current OpenJDK for both `aarch64` and `x86_64`. They are built for Termux's
 * own prefix (`/data/data/com.termux/files/usr`), which matters in exactly two
 * places, both handled:
 *
 *  - Their `DT_RUNPATH` points into that prefix. `LD_LIBRARY_PATH` is searched
 *    first, so ours wins — see [JavaServerBackend].
 *  - They depend on a few Termux libraries. Only three are load-bearing;
 *    see [DEPENDENCIES].
 */
object JavaRuntime {

    /** The launcher, renamed so the APK packager keeps it (`lib*.so` only). */
    private const val LAUNCHER = "libjavabin.so"

    /** Written after a successful install, so it happens once and not per launch. */
    private const val STAMP = ".complete"

    /** Termux's dependency libraries land here, kept apart from the JRE's own. */
    const val DEPS_DIR = "termux-lib"

    private const val REPO = "https://packages.termux.dev/apt/termux-main/pool/main"

    /**
     * One archive to fetch. [strip] is the path prefix inside it to discard —
     * Termux packages carry their whole absolute install path — and [into] is
     * the subdirectory of the runtime it unpacks to.
     */
    data class Archive(val url: String, val strip: String, val into: String = "")

    data class Spec(val javaMajor: Int, val abi: String, val jre: Archive, val deps: List<Archive>)

    /**
     * The libraries the runtime cannot start without, established by reading
     * `DT_NEEDED` across every `.so` it ships rather than by guessing:
     *
     *  - `libandroid-shmem`  — `libjvm.so`
     *  - `libandroid-spawn`  — `libjvm.so`, `libjava.so`
     *  - `zlib`              — `libzip.so`, `libjli.so` need `libz.so.1`, and
     *    Android's system `libz.so` does not carry that versioned soname
     *  - `libc++`            — `libandroid-spawn.so` itself needs
     *    `libc++_shared.so`. Transitive, and the reason this list is derived
     *    from a scan over the *closure* rather than the JRE alone: the first
     *    pass missed it and the VM would not load.
     *
     * Four more are referenced but only by things a headless server never
     * loads: `libasound` (sound), `libiconv` (JDWP and instrumentation),
     * `libjpeg` and `liblcms2` (imaging). Add them if a plugin ever needs one.
     */
    private val DEPENDENCIES = listOf(
        Triple("liba/libandroid-shmem", "libandroid-shmem", "0.7"),
        Triple("liba/libandroid-spawn", "libandroid-spawn", "0.3"),
        Triple("z/zlib", "zlib", "1.3.2"),
        Triple("libc/libc++", "libc++", "29"),
    )

    /** Pinned by version: an upstream bump must not silently change a build. */
    private val JDK_VERSIONS = mapOf(17 to "17.0.20", 21 to "21.0.12", 25 to "25.0.4")

    private val abi: String get() = Build.SUPPORTED_ABIS.firstOrNull() ?: "unknown"

    private fun specFor(javaMajor: Int, abi: String): Spec? {
        val version = JDK_VERSIONS[javaMajor] ?: return null
        return Spec(
            javaMajor = javaMajor,
            abi = abi,
            jre = Archive(
                url = "$REPO/o/openjdk-$javaMajor/openjdk-${javaMajor}_${version}_$abi.deb",
                strip = "data/data/com.termux/files/usr/lib/jvm/java-$javaMajor-openjdk",
            ),
            deps = DEPENDENCIES.map { (path, name, depVersion) ->
                Archive(
                    url = "$REPO/$path/${name}_${depVersion}_$abi.deb",
                    strip = "data/data/com.termux/files/usr/lib",
                    into = DEPS_DIR,
                )
            },
        )
    }

    /** The launcher, or null if this build ships none for the device's ABI. */
    fun launcher(context: Context): File? =
        File(context.applicationInfo.nativeLibraryDir, LAUNCHER).takeIf { it.canExecute() }

    fun isAvailable(context: Context): Boolean = launcher(context) != null

    fun supportedVersions(): List<Int> =
        JDK_VERSIONS.keys.filter { specFor(it, abi) != null }.sorted()

    fun home(context: Context, javaMajor: Int): File =
        File(context.filesDir, "runtimes/java$javaMajor")

    fun libjvm(context: Context, javaMajor: Int): File? =
        File(home(context, javaMajor), "lib/server/libjvm.so").takeIf { it.isFile }

    fun isInstalled(context: Context, javaMajor: Int): Boolean =
        File(home(context, javaMajor), STAMP).exists() && libjvm(context, javaMajor) != null

    /**
     * Ensure a runtime is present, downloading and unpacking if needed.
     *
     * Blocking — call it off the main thread. [onProgress] reports 0..1 across
     * the whole set of archives, or -1 while a size is unknown.
     */
    fun ensure(context: Context, javaMajor: Int, onProgress: (Float) -> Unit = {}): File {
        val target = home(context, javaMajor)
        if (isInstalled(context, javaMajor)) return target

        val spec = specFor(javaMajor, abi) ?: throw IllegalStateException(
            "No Java $javaMajor runtime is published for this device ($abi). " +
                "Available: ${supportedVersions().joinToString().ifEmpty { "none" }}."
        )

        // A partial install from an interrupted attempt must not look complete.
        target.deleteRecursively()
        target.mkdirs()

        val archives = listOf(spec.jre) + spec.deps
        try {
            archives.forEachIndexed { index, archive ->
                Log.i(TAG, "fetching ${archive.url}")
                val base = index.toFloat() / archives.size
                val span = 1f / archives.size
                fetch(archive, target) { fraction ->
                    onProgress(if (fraction < 0) -1f else base + fraction * span)
                }
            }
        } catch (err: Exception) {
            target.deleteRecursively()
            throw err
        }

        if (libjvm(context, javaMajor) == null) {
            target.deleteRecursively()
            throw IllegalStateException("The downloaded Java $javaMajor runtime has no libjvm.so.")
        }
        File(target, STAMP).writeText(spec.jre.url)
        Log.i(TAG, "Java $javaMajor ready at $target")
        return target
    }

    /** Streams straight into place: a phone should not need the space twice. */
    private fun fetch(archive: Archive, target: File, onProgress: (Float) -> Unit) {
        val connection = (URL(archive.url).openConnection() as HttpURLConnection).apply {
            instanceFollowRedirects = true
            connectTimeout = 30_000
            readTimeout = 60_000
        }
        try {
            if (connection.responseCode !in 200..299) {
                throw IllegalStateException(
                    "Download failed with HTTP ${connection.responseCode}: ${archive.url}"
                )
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

            val destination = if (archive.into.isEmpty()) target else File(target, archive.into)
            if (archive.url.endsWith(".deb")) {
                unpackDeb(counting.buffered(), destination, archive.strip)
            } else {
                TarArchiveInputStream(XZCompressorInputStream(counting.buffered()))
                    .use { untar(it, destination, archive.strip) }
            }
        } finally {
            connection.disconnect()
        }
    }

    /**
     * A `.deb` is an `ar` archive; the payload is its `data.tar.*` member.
     * Everything else (`debian-binary`, `control.tar.*`) is metadata.
     */
    private fun unpackDeb(input: InputStream, target: File, strip: String) {
        ArArchiveInputStream(input).use { ar ->
            while (true) {
                val entry = ar.nextEntry ?: break
                if (!entry.name.startsWith("data.tar")) continue
                val payload = if (entry.name.endsWith(".xz")) XZCompressorInputStream(ar) else ar
                TarArchiveInputStream(payload).use { untar(it, target, strip) }
                return
            }
        }
        throw IllegalStateException("That .deb has no data.tar member.")
    }

    private fun untar(tar: TarArchiveInputStream, target: File, strip: String) {
        val prefix = strip.trim('/')
        while (true) {
            val entry = tar.nextEntry ?: break
            val name = entry.name.removePrefix("./").trimStart('/')
            if (prefix.isNotEmpty() && !name.startsWith(prefix)) continue
            val relative = name.removePrefix(prefix).trimStart('/')
            if (relative.isEmpty()) continue

            val out = File(target, relative)
            // Archive entries are untrusted input even from a source you
            // trust; `..` would land files outside the target.
            if (!out.canonicalPath.startsWith(target.canonicalPath + File.separator)) {
                throw SecurityException("archive entry escapes the target: ${entry.name}")
            }

            when {
                entry.isDirectory -> out.mkdirs()
                entry.isSymbolicLink -> link(out, entry.linkName)
                else -> {
                    out.parentFile?.mkdirs()
                    out.outputStream().use(tar::copyTo)
                    // tar carries the execute bit; Java drops it, and the
                    // runtime's helper binaries need it back.
                    if (entry.mode and 0b001_000_000 != 0) out.setExecutable(true, false)
                }
            }
        }
    }

    /**
     * Symlinks matter here: zlib ships `libz.so.1` as a link to the real file,
     * and that is precisely the name `libzip.so` asks the linker for. Skipping
     * links would leave the JVM unable to read a jar.
     *
     * Falls back to copying, because some Android filesystems refuse links.
     */
    private fun link(out: File, linkName: String) {
        out.parentFile?.mkdirs()
        if (out.exists()) out.delete()
        try {
            Files.createSymbolicLink(out.toPath(), File(linkName).toPath())
        } catch (err: Exception) {
            val source = File(out.parentFile, linkName)
            if (source.isFile) {
                source.copyTo(out, overwrite = true)
            } else {
                Log.w(TAG, "could not materialise link ${out.name} -> $linkName: ${err.message}")
            }
        }
    }

    private const val TAG = "HomerunJava"
}
