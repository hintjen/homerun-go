package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.util.zip.ZipFile

/**
 * Mod loaders, which install by being run rather than downloaded.
 *
 * # Why this is not [ServerJar]
 *
 * Vanilla and Paper publish a **server jar**: resolve a URL, download it,
 * check a digest, launch it. Fabric, Quilt, Forge and NeoForge publish an
 * **installer** — a jar run once that fetches what it needs and leaves a
 * launchable server behind. The two share a version resolver and nothing else.
 *
 * `Core.loaderIsInstalled` is the question that decides which path a server
 * takes, and it is the core's to answer so the host has no loader list of its
 * own to drift.
 *
 * # Parity with the desktop
 *
 * `setupServerLoader` in `mod-installer.ts` is the spec, and
 * `.homerun-loader.json` is deliberately the same name and shape — a server
 * directory restored from a desktop backup is understood rather than
 * reinstalled.
 *
 * Every decision here is `homerun_core::minecraft::loader`'s: which installer,
 * whether what is installed can be kept, what to delete when it cannot. This
 * file makes the requests, runs the installer and moves the files.
 *
 * # What it does not do yet
 *
 * Mods. A Fabric server starts with no mods on it until M4 of
 * `plans/android-mod-loaders.md` lands the resolver — which is also what will
 * finally give a Paper server its plugins, since nothing installs those today
 * either.
 */
object ServerLoader {

    /**
     * Removed as soon as it has run; nothing should ever launch it.
     *
     * One name for every loader, and deliberately not matching the sweep's
     * `forge-*.jar` pattern — `Core.loaderFilesToClean` excludes anything with
     * "installer" in the name so a failed install cannot delete the installer
     * it was about to run, and this name keeps that true by construction.
     */
    private const val INSTALLER_NAME = "_loader-installer.jar"

    /** What the Fabric and Quilt installers produce alongside their launch jar. */
    private const val SERVER_JAR = "server.jar"


    /**
     * Where a bundled server jar states the Java it needs. Mojang's manifest
     * can disagree with the jar, and the jar is the one that fails.
     */
    private const val BUNDLER_CLASS = "net/minecraft/bundler/Main.class"

    /**
     * Generous on purpose. The desktop allows Forge ten minutes; this is a
     * phone, quite possibly on mobile data, downloading a loader and its
     * libraries. It is a backstop against a wedged installer, not a budget —
     * `native-server-start` still has no bridge timeout.
     */
    private const val INSTALL_TIMEOUT_MS = 15 * 60 * 1000L

    private const val CONNECT_TIMEOUT_MS = 30_000
    private const val READ_TIMEOUT_MS = 60_000

    private const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

    private val json = Json { ignoreUnknownKeys = true }

    /** Everything needed to run a JVM, so an install can have one. */
    data class Runtime(
        val launcher: File,
        val javaHome: File,
        val libjvm: File,
        val tmpDir: File,
    )

    /**
     * Install [loader] into [dir] if it is not already there, and return the
     * jar to launch.
     *
     * Does nothing when the installed loader already matches, so a restart is
     * free — the same property [ServerJar.ensure] has for a downloaded jar.
     *
     * Blocking and potentially minutes long. Cancellable throughout: a stop
     * during an install destroys the installer process rather than waiting for
     * it, which is what the desktop achieves by racing every step against the
     * launch's `AbortController`.
     */
    suspend fun ensure(
        dir: File,
        loader: String,
        mcVersion: String,
        loaderVersion: String?,
        runtime: Runtime,
        onLog: (String) -> Unit,
    ): Installed = withContext(Dispatchers.IO) {
        // Fabric leaves a launch jar; Forge and NeoForge leave argfiles and no
        // jar at all. The core says which, so nothing here holds a loader list.
        val launchName = Core.loaderLaunchJar(loader)
        val marker = LoaderMarker.read(dir)

        // Forge and NeoForge pin themselves to a build, and which build is a
        // question for their maven metadata rather than a fixed URL — so it is
        // resolved before change-detection, because "the newest build moved"
        // is exactly the kind of change that must force a reinstall.
        val resolvedLoaderVersion = if (launchName == null) {
            resolveBuild(loader, mcVersion, loaderVersion)
        } else {
            loaderVersion
        }

        // Two questions, and both have to say no. The marker can be right
        // while the files are gone — a failed install, or a restore that
        // brought the marker and not the tree — and believing it then would
        // launch something that is not there.
        val stale = Core.loaderNeedsReinstall(marker, loader, mcVersion, resolvedLoaderVersion)
        if (!stale) {
            landed(dir, loader, launchName)?.let { already ->
                Log.i(TAG, "$loader for Minecraft $mcVersion is already installed")
                onLog("[Homerun] ${label(loader, mcVersion)} is already installed.")
                return@withContext already
            }
        }

        // Quilt trails Minecraft by weeks, and its installer does not fail in a
        // way anyone could act on when asked for a version it cannot map. So
        // ask first.
        //
        // Deliberately **before** `clean`, which the desktop is not: it deletes
        // the launch jar and then finds out. Refusing here leaves a server that
        // still works exactly as it did, which matters most for the case this
        // fires on — a Minecraft version bumped to one Quilt has not reached.
        if (loader == "quilt") {
            Core.ensureQuiltSupports(mcVersion, quiltMappings(mcVersion))
        }

        if (marker != null && stale) {
            onLog("[Homerun] The installed loader changed — reinstalling.")
        }
        clean(dir)

        onLog("[Homerun] Installing ${label(loader, mcVersion)}...")
        val installer = File(dir, INSTALLER_NAME)
        try {
            // Forge and NeoForge name their installer by version. Fabric and
            // Quilt each publish an index and the core picks from it — by
            // different rules, which is why these are two calls and not one
            // with a parameter: Quilt's index marks no entry stable, so
            // Fabric's rule would silently fall through to "first" forever.
            val url = when {
                launchName == null -> Core.loaderInstallerUrl(loader, resolvedLoaderVersion!!)
                loader == "quilt" -> Core.quiltInstallerUrl(fetchJson(Core.QUILT_INSTALLER_META))
                else -> Core.fabricInstallerUrl(fetchJson(Core.FABRIC_INSTALLER_META))
            }
            download(url, installer, loader.replaceFirstChar(Char::uppercase))
            runInstaller(installer, dir, loader, mcVersion, loaderVersion, runtime, onLog)
        } finally {
            // Whatever happened, this must not survive: a stray runnable jar
            // in the server directory is exactly what `filesToClean`'s
            // `installer` exclusion is protecting, and leaving it there would
            // make a later sweep ambiguous.
            installer.delete()
        }

        val result = landed(dir, loader, launchName)
            ?: throw ServerBackendException.Engine(
                "The ${label(loader, mcVersion)} installer finished but left nothing to " +
                    "launch. The install did not take."
            )
        LoaderMarker.putLoader(dir, loader, mcVersion, resolvedLoaderVersion)
        onLog("[Homerun] ${label(loader, mcVersion)} ready.")
        result
    }

    /**
     * What an installed loader launches.
     *
     * Two shapes, because the loaders genuinely differ. Fabric produces a jar
     * whose manifest names its main class and its libraries. Forge and
     * NeoForge produce **no runnable jar at all** — their argfile carries the
     * module path, the main class and the program arguments, and the
     * `server.jar` in their directory is a placeholder nothing runs.
     */
    sealed interface Installed {
        data class LaunchJar(val jar: File) : Installed
        data class Argfiles(val expanded: Core.Expanded) : Installed
    }

    /**
     * What is actually on disk after (or before) an install, or null if the
     * install has not landed.
     *
     * For Fabric that is two files. For Forge and NeoForge it is a run script
     * naming argfiles that exist and expand to a main class — checking the
     * script alone would accept an install whose argfile the sweep removed.
     */
    private fun landed(dir: File, loader: String, launchName: String?): Installed? {
        if (launchName != null) {
            val jar = File(dir, launchName)
            return if (jar.isFile && File(dir, SERVER_JAR).isFile) {
                Installed.LaunchJar(jar)
            } else {
                null
            }
        }
        return expandArgfiles(dir)?.let(Installed::Argfiles)
    }

    /**
     * Read this server's argfiles and turn them into a launch.
     *
     * The run script is read rather than guessed because the loader build is
     * in the argfile's path — `libraries/net/neoforged/neoforge/21.4.157/…` —
     * so guessing means knowing the build, and the script already knows it.
     *
     * **`user_jvm_args.txt` is deliberately not read.** The desktop needs it
     * because it invokes the `java` binary and that is the only way to hand it
     * a heap; this host passes `homerun-core`'s heap options straight to the
     * VM, so reading the file as well would set `-Xmx` twice. The generated
     * one is all comments in any case — NeoForge ships it with every line
     * commented out and an invitation to uncomment `-Xmx4G`, and there is no
     * way to edit a file inside app-private storage on a phone.
     */
    fun expandArgfiles(dir: File): Core.Expanded? {
        val present = dir.list()?.toList() ?: return null
        val script = Core.preferredRunScript(present) ?: return null
        val named = Core.referencedArgfiles(File(dir, script).readTextOrNull() ?: return null)

        val paths = named.ifEmpty {
            // No run script named one, so fall back to walking `libraries/`.
            // Same preference, same reason: the Windows argfile's class path
            // uses `;` and `\`, which on Android resolves to nothing.
            listOfNotNull(Core.fallbackArgfile(argfileCandidates(dir)))
        }
        if (paths.isEmpty()) return null

        val contents = paths.map { File(dir, it).readTextOrNull() ?: return null }
        return Core.expandArgfiles(contents).takeIf { it.mainClass != null }
    }

    /** Every `*_args.txt` under `libraries/`, as paths relative to [dir]. */
    private fun argfileCandidates(dir: File): List<String> {
        val root = File(dir, "libraries")
        if (!root.isDirectory) return emptyList()
        return root.walkTopDown()
            .filter { it.isFile && it.name.endsWith("_args.txt") }
            .map { it.relativeTo(dir).invariantSeparatorsPath }
            .toList()
    }

    private fun File.readTextOrNull(): String? = runCatching { readText() }.getOrNull()

    private suspend fun resolveBuild(
        loader: String,
        mcVersion: String,
        pinned: String?,
    ): String = withContext(Dispatchers.IO) {
        val xml = runCatching { fetchText(Core.loaderMetadataUrl(loader)) }.getOrElse {
            throw ServerBackendException.Engine(
                "Could not reach ${label(loader, mcVersion)}'s version list: " +
                    (it.message ?: "no connection")
            )
        }
        runCatching { Core.resolveLoaderVersion(loader, xml, mcVersion, pinned) }
            .getOrElse {
                throw ServerBackendException.Engine(it.message ?: "No build for that version.")
            }
    }

    /**
     * The Java major the installed server jar actually needs, or null.
     *
     * Mojang's manifest states a version and the bundled jar can disagree with
     * it — the desktop re-resolves its JDK when they differ
     * (`mod-installer.ts:790`). This build has two runtimes and has already
     * chosen one by the time the installer produces a jar to inspect, so the
     * caller re-selects.
     */
    fun bundlerJavaMajor(dir: File): Int? = runCatching {
        ZipFile(File(dir, SERVER_JAR)).use { zip ->
            val entry = zip.getEntry(BUNDLER_CLASS) ?: return null
            // The class-file header is the first eight bytes and nothing else
            // here is needed, so nothing else is read.
            val head = ByteArray(8)
            zip.getInputStream(entry).use { input ->
                var read = 0
                while (read < head.size) {
                    val n = input.read(head, read, head.size - read)
                    if (n < 0) return null
                    read += n
                }
            }
            Core.bundlerJavaMajor(head)
        }
    }.getOrNull()

    // -----------------------------------------------------------------------
    // Installing
    // -----------------------------------------------------------------------

    private suspend fun runInstaller(
        installer: File,
        dir: File,
        loader: String,
        mcVersion: String,
        loaderVersion: String?,
        runtime: Runtime,
        onLog: (String) -> Unit,
    ) {
        val mainClass = JavaProcess.mainClassOf(installer)
            ?: throw ServerBackendException.Engine(
                "The ${label(loader, mcVersion)} installer names no main class, so it " +
                    "cannot be run."
            )

        // Three command lines, because three installers.
        //
        // Fabric's and Forge's are the desktop's arguments verbatim
        // (`mod-installer.ts:783`, `:820`). Fabric's `-downloadMinecraft` is
        // what makes it fetch the vanilla server jar itself, which is why that
        // path never goes through `ServerJar` for the jar it launches. Forge and
        // NeoForge take `--installServer` and nothing else — the version is
        // baked into the installer, which is why the build had to be resolved
        // before the URL.
        //
        // Quilt's is **not** Fabric's, despite Quilt being a Fabric fork that
        // produces the same kind of launch jar. Its Minecraft and loader
        // versions are positional, its directory is a joined `--install-dir=`,
        // and its download flag is `--download-server`. Taken from
        // `quilt-installer help` run on a device, not from the family
        // resemblance — which would have failed on the very first argument.
        val args = when (loader) {
            "fabric" -> buildList {
                add("server")
                add("-mcversion"); add(mcVersion)
                loaderVersion?.let { add("-loader"); add(it) }
                add("-dir"); add(dir.absolutePath)
                add("-downloadMinecraft")
            }
            "quilt" -> buildList {
                add("install"); add("server")
                add(mcVersion)
                // Positional, and it has to stay immediately after the
                // Minecraft version — there is no flag that can carry it.
                loaderVersion?.let { add(it) }
                add("--install-dir=${dir.absolutePath}")
                add("--download-server")
            }
            else -> listOf("--installServer")
        }

        JavaProcess.runOrThrow(
            invocation = JavaProcess.invocation(
                launcher = runtime.launcher,
                javaHome = runtime.javaHome,
                libjvm = runtime.libjvm,
                classpath = listOf(installer),
                mainClass = mainClass,
                programArgs = args,
                workDir = dir,
                tmpDir = runtime.tmpDir,
            ),
            timeoutMs = INSTALL_TIMEOUT_MS,
            what = "The ${label(loader, mcVersion)} installer",
            onLog = { line -> onLog("[${label(loader, mcVersion).substringBefore(' ')}] $line") },
        )
    }

    /**
     * What Quilt says it has mapped for [mcVersion], or null if we could not
     * ask.
     *
     * A request that failed is handed to the core as `null` rather than thrown,
     * so the refusal is worded once, in one place — and so an unreachable
     * meta.quiltmc.org reads as "cannot confirm Quilt supports this" rather
     * than as a connection error the player has to interpret. The desktop draws
     * the same line with `catch (_) { quiltSupported = false }`.
     *
     * This only ever runs when an install is about to happen, which needs the
     * network anyway — so it costs a working server nothing.
     */
    private fun quiltMappings(mcVersion: String): JsonElement = try {
        fetchJson(Core.quiltIntermediaryUrl(mcVersion))
    } catch (err: Exception) {
        Log.w(TAG, "could not read Quilt's mappings for $mcVersion: ${err.message}")
        JsonNull
    }

    private fun fetchText(url: String): String {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
        }
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            return connection.inputStream.bufferedReader().use { it.readText() }
        } finally {
            connection.disconnect()
        }
    }

    /**
     * Remove what a previous loader install left, so the next one starts clean.
     *
     * The list is the core's and covers loaders this build cannot host: a
     * directory restored from a desktop backup can carry a Forge install, and
     * switching it to Fabric has to clear those jars or the next start finds
     * two servers to run.
     */
    private fun clean(dir: File) {
        val entries = dir.list()?.toList() ?: emptyList()
        for (name in Core.loaderFilesToClean(entries)) {
            val target = File(dir, name)
            if (!target.exists()) continue
            Log.i(TAG, "clearing ${target.name} before installing")
            if (target.isDirectory) target.deleteRecursively() else target.delete()
        }
    }

    private fun label(loader: String, mcVersion: String) =
        "${loader.replaceFirstChar(Char::uppercase)} for Minecraft $mcVersion"

    // -----------------------------------------------------------------------
    // Transfer
    // -----------------------------------------------------------------------

    /**
     * [what] names the installer in any failure, because this runs for four
     * loaders now and "the Fabric installer" was already the wrong words when a
     * NeoForge download failed.
     */
    private fun download(url: String, dest: File, what: String) {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
        }
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw ServerBackendException.Engine(
                    "Could not download the $what installer (HTTP ${connection.responseCode})."
                )
            }
            dest.parentFile?.mkdirs()
            connection.inputStream.use { input ->
                dest.outputStream().use { input.copyTo(it) }
            }
        } catch (err: IOException) {
            throw ServerBackendException.Engine(
                "Could not download the $what installer: ${err.message ?: "no connection"}"
            )
        } finally {
            connection.disconnect()
        }
    }

    private fun fetchJson(url: String): JsonElement {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
            setRequestProperty("Accept", "application/json")
        }
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw ServerBackendException.Engine(
                    "Could not reach the Fabric version servers (HTTP ${connection.responseCode})."
                )
            }
            return json.parseToJsonElement(
                connection.inputStream.bufferedReader().use { it.readText() }
            )
        } catch (err: IOException) {
            throw ServerBackendException.Engine(
                "Could not reach the Fabric version servers: ${err.message ?: "no connection"}"
            )
        } finally {
            connection.disconnect()
        }
    }

    private const val TAG = "HomerunJava"
}
