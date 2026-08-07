package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

/**
 * The server jar: which one to run, and getting it onto the device.
 *
 * # Why this may be downloaded when the JRE may not
 *
 * Google Play forbids downloading *executable code* from outside Play, which
 * is why [JavaRuntime] ships inside the APK. A server jar is not that: it is
 * data read by a virtual machine, which is the carve-out the policy names
 * explicitly. Anvil-MC ships the same split — see `docs/android-server-backend.md`.
 *
 * # Parity with the desktop
 *
 * `mod-installer.ts` in the `homerun` repo is the spec. Same endpoints, same
 * "resolve the Mojang manifest first" order (it names the required Java for
 * every loader, not just vanilla), same policy of never re-downloading a jar
 * that is already correct.
 *
 * Two deliberate differences, both because this is a phone:
 *
 *  - **Downloads resume.** A 55 MB pull over mobile data should survive a
 *    tunnel. The desktop starts over.
 *  - **Every download is checksum-verified.** The desktop verifies vanilla
 *    only, and Paper publishes a SHA-256 it was already fetching and
 *    discarding.
 */
object ServerJar {

    /** Named for the desktop's convention; the JVM is handed a classpath, not `-jar`. */
    private const val JAR_NAME = "server.jar"

    /** Records what [JAR_NAME] actually is, so a version change re-downloads. */
    private const val META_NAME = "homerun-jar.json"

    private const val VERSION_MANIFEST =
        "https://launchermeta.mojang.com/mc/game/version_manifest.json"

    private const val PAPER_BUILDS = "https://fill.papermc.io/v3/projects/paper/versions"

    /**
     * Honest, and accepted by every endpoint here — checked against Mojang and
     * PaperMC. The desktop spoofs Chrome; that was for endpoints that 403 an
     * unfamiliar agent, and none of these do. PaperMC's docs ask for a
     * descriptive one.
     */
    private const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

    private const val CONNECT_TIMEOUT_MS = 30_000
    private const val READ_TIMEOUT_MS = 60_000

    /** Resume makes a retry cheap, so the backoff can stay short. */
    private val RETRY_DELAYS_MS = listOf(2_000L, 5_000L, 10_000L)

    private val json = Json { ignoreUnknownKeys = true }

    /** A digest the publisher gave us, and the algorithm that produced it. */
    data class Checksum(val algorithm: String, val hex: String)

    /** One downloadable server jar. */
    data class Artifact(
        val url: String,
        val loader: String,
        val version: String,
        val checksum: Checksum?,
        /** From Mojang's version metadata — the class-file level the jar needs. */
        val requiredJava: Int,
        val sizeBytes: Long?,
    )

    /** What is on disk right now. */
    @Serializable
    private data class JarMeta(
        val loader: String,
        val version: String,
        val checksum: String? = null,
    )

    /**
     * Put the right server jar in [dir] and return it.
     *
     * Does nothing when the jar there is already the one asked for, so a
     * restart is free. [bundledJava] is [JavaRuntime.javaMajor]; when it is
     * lower than the jar needs this fails with that stated, rather than
     * letting the JVM die on `UnsupportedClassVersionError`.
     *
     * Blocking and potentially minutes long — `native-server-start` has no
     * bridge timeout for exactly this reason.
     */
    suspend fun ensure(
        dir: File,
        version: String?,
        loader: String,
        bundledJava: Int?,
        onLog: (String) -> Unit,
    ): File = withContext(Dispatchers.IO) {
        val jar = File(dir, JAR_NAME)
        val onDisk = readMeta(dir)

        val artifact = try {
            resolve(version, loader)
        } catch (err: ServerBackendException) {
            throw err // An unsupported loader is a verdict, not a network blip.
        } catch (err: Exception) {
            // Hosting offline is a real thing to want: a phone on a LAN with
            // no internet can still serve the world it already has. Only fail
            // when there is nothing on disk to fall back to.
            if (jar.isFile && onDisk != null && onDisk.couldSatisfy(version, loader)) {
                Log.w(TAG, "version lookup failed (${err.message}) — using the jar on disk")
                onLog("[Homerun] Could not reach the version servers — starting the Minecraft ${onDisk.version} jar already downloaded.")
                return@withContext jar
            }
            throw ServerBackendException.Engine(
                "Could not look up the $loader server for Minecraft ${version ?: "latest"}: " +
                    (err.message ?: "no connection")
            )
        }

        if (bundledJava != null && artifact.requiredJava > bundledJava) {
            throw ServerBackendException.Engine(
                "Minecraft ${artifact.version} needs Java ${artifact.requiredJava}, and this " +
                    "version of Homerun ships Java $bundledJava. Update the app, or choose an " +
                    "older Minecraft version."
            )
        }

        if (jar.isFile && onDisk != null && onDisk.satisfies(artifact)) {
            Log.i(TAG, "${artifact.loader} ${artifact.version} already downloaded")
            return@withContext jar
        }

        val size = artifact.sizeBytes?.let { " (${it / 1024 / 1024} MB)" } ?: ""
        onLog("[Homerun] Downloading ${label(artifact)}$size...")

        // A jar that no longer matches is dead weight on a device where space
        // is scarce — and leaving it would let a failed download start a
        // server on the wrong version.
        if (jar.exists()) jar.delete()
        File(dir, META_NAME).delete()

        var lastReported = -1
        withRetries { attempt ->
            if (attempt > 0) onLog("[Homerun] Download interrupted — resuming...")
            download(artifact, jar) { done, total ->
                val percent = if (total > 0) ((done * 100) / total).toInt() else -1
                // Every percent would be 100 lines through the bridge and into
                // a console the user is reading. Every fifth is enough to show
                // it is moving.
                if (percent >= 0 && percent >= lastReported + 5) {
                    lastReported = percent
                    onLog("[Homerun] Downloading ${label(artifact)}... $percent%")
                }
            }
        }

        writeMeta(dir, JarMeta(artifact.loader, artifact.version, artifact.checksum?.hex))
        onLog("[Homerun] ${label(artifact)} ready.")
        jar
    }

    private fun label(artifact: Artifact): String =
        if (artifact.loader == "vanilla") "Minecraft ${artifact.version}"
        else "${artifact.loader.replaceFirstChar(Char::uppercase)} for Minecraft ${artifact.version}"

    // -----------------------------------------------------------------------
    // Resolution
    // -----------------------------------------------------------------------

    /**
     * Mojang's manifest is consulted for every loader, not just vanilla: it is
     * what turns "latest" into a concrete version and it is the only source
     * for the Java level the jar needs.
     */
    private fun resolve(version: String?, loader: String): Artifact {
        val vanilla = resolveVanilla(version)
        return when (loader.lowercase().ifBlank { "vanilla" }) {
            "vanilla" -> vanilla
            "paper" -> resolvePaper(vanilla)
            else -> throw ServerBackendException.Engine(
                "Homerun for Android cannot host $loader servers yet — those install by running " +
                    "an installer at setup time. Vanilla and Paper both work, and Paper runs " +
                    "Bukkit and Spigot plugins."
            )
        }
    }

    private fun resolveVanilla(version: String?): Artifact {
        val manifest = fetchJson(VERSION_MANIFEST).jsonObject
        val target = version?.takeIf { it.isNotBlank() && !it.equals("LATEST", true) }
            ?: manifest["latest"]?.jsonObject?.get("release")?.jsonPrimitive?.contentOrNull
            ?: throw IOException("the version manifest names no latest release")

        val entry = manifest["versions"]?.jsonArray
            ?.firstOrNull { it.jsonObject["id"]?.jsonPrimitive?.contentOrNull == target }
            ?.jsonObject
            ?: throw IOException("Minecraft $target is not in the version manifest")

        val url = entry["url"]?.jsonPrimitive?.contentOrNull
            ?: throw IOException("Minecraft $target has no metadata URL")
        val meta = fetchJson(url).jsonObject

        val server = meta["downloads"]?.jsonObject?.get("server")?.jsonObject
            ?: throw IOException("Minecraft $target publishes no server download")

        return Artifact(
            url = server["url"]?.jsonPrimitive?.contentOrNull
                ?: throw IOException("Minecraft $target has no server jar URL"),
            loader = "vanilla",
            version = target,
            checksum = server["sha1"]?.jsonPrimitive?.contentOrNull?.let { Checksum("SHA-1", it) },
            // Everything before 1.17 predates the field and runs on anything
            // modern; 21 is the desktop's floor for the same reason.
            requiredJava = meta["javaVersion"]?.jsonObject?.get("majorVersion")
                ?.jsonPrimitive?.intOrNull ?: 21,
            sizeBytes = server["size"]?.jsonPrimitive?.longOrNull,
        )
    }

    /**
     * Paper for the same Minecraft version.
     *
     * **Builds come back newest-first**, and the array also carries every
     * experimental build ever cut for the version — so this picks the highest
     * *stable* id rather than trusting position. (The desktop takes the last
     * element, which on this API is build 1, an alpha.)
     */
    private fun resolvePaper(vanilla: Artifact): Artifact {
        val builds = fetchJson("$PAPER_BUILDS/${vanilla.version}/builds").jsonArray
            .map { it.jsonObject }
        if (builds.isEmpty()) {
            throw ServerBackendException.Engine(
                "Paper has no build for Minecraft ${vanilla.version} yet."
            )
        }

        fun id(build: JsonObject) = build["id"]?.jsonPrimitive?.intOrNull ?: -1

        val stable = builds.filter {
            it["channel"]?.jsonPrimitive?.contentOrNull.equals("STABLE", ignoreCase = true)
        }
        val build = (stable.ifEmpty { builds }).maxByOrNull(::id)
            ?: throw IOException("no usable Paper build for ${vanilla.version}")

        val download = build["downloads"]?.jsonObject?.get("server:default")?.jsonObject
            ?: throw IOException("Paper build ${id(build)} publishes no server download")

        return Artifact(
            url = download["url"]?.jsonPrimitive?.contentOrNull
                ?: throw IOException("Paper build ${id(build)} has no download URL"),
            loader = "paper",
            version = vanilla.version,
            checksum = download["checksums"]?.jsonObject?.get("sha256")
                ?.jsonPrimitive?.contentOrNull?.let { Checksum("SHA-256", it) },
            requiredJava = vanilla.requiredJava,
            sizeBytes = download["size"]?.jsonPrimitive?.longOrNull,
        )
    }

    // -----------------------------------------------------------------------
    // Transfer
    // -----------------------------------------------------------------------

    /**
     * Fetch [artifact] to [dest], resuming a partial file if one is there.
     *
     * The partial is **named after the artifact's own digest**, so a resume
     * can only ever continue the file it began. That is what makes this safe
     * without the ETag bookkeeping the desktop needs: changing version changes
     * the name, so bytes from one jar can never be appended to another.
     */
    private fun download(artifact: Artifact, dest: File, onProgress: (Long, Long) -> Unit) {
        val part = File(dest.parentFile, "${artifact.checksum?.hex ?: "server"}.jar.part")
        val have = if (part.isFile) part.length() else 0L

        val connection = (URL(artifact.url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
            if (have > 0) setRequestProperty("Range", "bytes=$have-")
        }

        try {
            val code = connection.responseCode
            val resuming = code == HttpURLConnection.HTTP_PARTIAL
            if (code != HttpURLConnection.HTTP_OK && !resuming) {
                throw IOException("HTTP $code from ${artifact.url}")
            }

            val remaining = connection.contentLengthLong.coerceAtLeast(0L)
            val total = if (resuming) have + remaining else artifact.sizeBytes ?: remaining
            var done = if (resuming) have else 0L

            // append=false truncates, which is what a 200 after a partial means:
            // the server ignored the Range and is sending the whole file again.
            FileOutputStream(part, resuming).use { out ->
                connection.inputStream.use { input ->
                    val buffer = ByteArray(64 * 1024)
                    while (true) {
                        val read = input.read(buffer)
                        if (read < 0) break
                        out.write(buffer, 0, read)
                        done += read
                        onProgress(done, total)
                    }
                }
            }
        } finally {
            connection.disconnect()
        }

        artifact.checksum?.let { expected ->
            val actual = digest(part, expected.algorithm)
            if (!actual.equals(expected.hex, ignoreCase = true)) {
                // Not retried. A digest mismatch is a corrupt or substituted
                // file, not a transient failure — the desktop draws the same
                // line. Deleting it means the next attempt starts clean.
                part.delete()
                throw ServerBackendException.Engine(
                    "The downloaded ${label(artifact)} did not match its published checksum, " +
                        "so it was discarded. Try again."
                )
            }
        }

        if (!part.renameTo(dest)) {
            throw IOException("could not move the downloaded jar into place")
        }
    }

    private fun digest(file: File, algorithm: String): String {
        val md = MessageDigest.getInstance(algorithm)
        file.inputStream().use { input ->
            val buffer = ByteArray(64 * 1024)
            while (true) {
                val read = input.read(buffer)
                if (read < 0) break
                md.update(buffer, 0, read)
            }
        }
        return md.digest().joinToString("") { "%02x".format(it) }
    }

    /** Retry transient failures only; a [ServerBackendException] is a verdict. */
    private suspend fun withRetries(block: suspend (attempt: Int) -> Unit) {
        for (attempt in 0..RETRY_DELAYS_MS.size) {
            try {
                return block(attempt)
            } catch (err: ServerBackendException) {
                throw err
            } catch (err: IOException) {
                if (attempt == RETRY_DELAYS_MS.size) {
                    throw ServerBackendException.Engine(
                        "The server jar could not be downloaded: ${err.message ?: "no connection"}"
                    )
                }
                Log.w(TAG, "download attempt ${attempt + 1} failed: ${err.message}")
                delay(RETRY_DELAYS_MS[attempt])
            }
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
                throw IOException("HTTP ${connection.responseCode} from $url")
            }
            return json.parseToJsonElement(
                connection.inputStream.bufferedReader().use { it.readText() }
            )
        } finally {
            connection.disconnect()
        }
    }

    // -----------------------------------------------------------------------
    // On-disk record
    // -----------------------------------------------------------------------

    private fun JarMeta.satisfies(artifact: Artifact): Boolean =
        loader == artifact.loader &&
            version == artifact.version &&
            // Null on both sides means neither publisher gave us a digest, so
            // loader and version are all there is to compare.
            checksum == artifact.checksum?.hex

    /** Loose enough for the offline fallback: any build of the right thing. */
    private fun JarMeta.couldSatisfy(version: String?, loader: String): Boolean =
        this.loader == loader.lowercase().ifBlank { "vanilla" } &&
            (version.isNullOrBlank() || version.equals("LATEST", true) || this.version == version)

    private fun readMeta(dir: File): JarMeta? = runCatching {
        json.decodeFromString<JarMeta>(File(dir, META_NAME).readText())
    }.getOrNull()

    private fun writeMeta(dir: File, meta: JarMeta) {
        runCatching { File(dir, META_NAME).writeText(json.encodeToString(meta)) }
            .onFailure { Log.w(TAG, "could not record the jar version: ${it.message}") }
    }

    private const val TAG = "HomerunJava"
}
