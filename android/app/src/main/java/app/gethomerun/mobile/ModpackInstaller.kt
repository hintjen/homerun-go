package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.zip.ZipEntry
import java.util.zip.ZipFile

/**
 * Installing a Modrinth modpack onto a phone.
 *
 * # What this does and what the core does
 *
 * A `.mrpack` is a zip: `modrinth.index.json` naming mods to fetch by URL,
 * plus an `overrides/` tree copied verbatim into the server directory. This
 * file fetches the archive, reads the zip, hashes what needs hashing, writes
 * files and deletes files.
 *
 * **Which mods must not be installed is entirely the core's** —
 * `homerun_core::minecraft::modpack`. That question is much harder than it
 * sounds and `native-mod-support.md` is the record of why: the manifest's own
 * `env.server` is author-supplied and routinely wrong, dropping every
 * client-only mod breaks servers a different way because kept mods hard-depend
 * on client-only libraries, and packs ship CurseForge builds that appear on
 * Modrinth not at all.
 *
 * # What a pack overrides
 *
 * The pack's manifest decides the loader, the Minecraft version and the loader
 * *build* — not the server's `TYPE` and `VERSION`. The desktop does the same,
 * running `setupModrinthModpack` before `setupServerLoader` and using what it
 * returned. A pack pinned to Forge `47.2.17` and run on `47.4.20` dies at boot
 * with a mixin `InjectionError`, so the pin is not advisory.
 *
 * # Mobile-specific
 *
 * Two things this does that the desktop does not, both because it is a phone:
 * the archive is cached by version id so a restart re-downloads nothing, and
 * **free space is checked before the mods are fetched**. A kitchen-sink pack
 * plus a world can exceed what a device has, and failing at 90% having filled
 * the storage is the worst available outcome.
 */
object ModpackInstaller {

    private const val MANIFEST = "modrinth.index.json"
    private const val OVERRIDES = "overrides/"
    private const val MOD_DIR = "mods/"

    /** Fabric's and Quilt's metadata, and Forge's two. */
    private val FABRIC_META = listOf("fabric.mod.json", "quilt.mod.json")
    private val TOML_META = listOf("META-INF/neoforge.mods.toml", "META-INF/mods.toml")

    private const val ENV_MODPACK = "MODRINTH_MODPACK"
    private const val ENV_EXCLUDE_FILES = "MODRINTH_EXCLUDE_FILES"
    private const val ENV_OVERRIDES_EXCLUSIONS = "MODRINTH_OVERRIDES_EXCLUSIONS"

    private const val CONNECT_TIMEOUT_MS = 30_000
    private const val READ_TIMEOUT_MS = 60_000
    private const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

    /**
     * Headroom to leave after a pack is installed.
     *
     * A world grows, the JVM writes logs, and restic needs somewhere to stage
     * a snapshot. Filling the last byte of a phone breaks more than this app.
     */
    private const val FREE_SPACE_MARGIN_BYTES = 512L * 1024 * 1024

    private val json = Json { ignoreUnknownKeys = true }

    /** What the pack turned out to require, and what it placed. */
    data class Pack(
        val loader: String,
        val mcVersion: String,
        val loaderVersion: String?,
        /** `modpackFiles`: what the sweep must not treat as stale. */
        val files: List<String>,
        /** `modpackProjects`: what `mods` must not install a second time. */
        val projects: List<String>,
    )

    /** The `MODRINTH_MODPACK` value, or null when this server is not a pack. */
    fun configured(env: JsonObject?): String? =
        env?.get(ENV_MODPACK)?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() }

    /**
     * Resolve, fetch and install a pack into [dir].
     *
     * Throws [ServerBackendException.Engine] when the pack cannot be installed
     * — unlike [ModInstaller], which never fails a launch. The difference is
     * deliberate: a missing mod leaves a server that still runs, and a missing
     * *modpack* leaves one that is not the server the player asked for.
     */
    suspend fun install(
        dir: File,
        cacheDir: File,
        modpack: String,
        env: JsonObject?,
        onLog: (String) -> Unit,
    ): Pack = withContext(Dispatchers.IO) {
        onLog("[Homerun] Resolving modpack...")
        val source = resolve(modpack)
        val archive = File(cacheDir.apply { mkdirs() }, "${source.second}.mrpack")

        if (!archive.isFile) {
            onLog("[Homerun] Downloading the modpack...")
            download(source.first, archive)
        }

        ZipFile(archive).use { zip ->
            val manifest = readManifest(zip)
            val requires = Core.modpackRequires(manifest)
            val loader = requires["loader"]!!.jsonPrimitive.content
            val mcVersion = requires["mcVersion"]!!.jsonPrimitive.content
            val loaderVersion = requires["loaderVersion"]?.jsonPrimitive?.contentOrNull

            onLog("[Homerun] The pack needs $loader for Minecraft $mcVersion.")

            val manifestMods = manifestMods(manifest)
            requireSpace(dir, manifestMods, onLog)

            val overrideMods = overrideMods(zip)
            val outcome = decide(manifestMods, overrideMods, env)

            for (note in outcome["notes"]?.jsonArray.orEmpty()) {
                onLog("[Homerun] ${note.jsonPrimitive.content}")
            }

            val modDir = File(dir, "mods").apply { mkdirs() }
            val skip = outcome.strings("skipOverrides").toSet()
            extractOverrides(zip, dir, skip, exclusions(env), onLog)
            fetchManifestMods(outcome, modDir, onLog)

            for (name in outcome.strings("remove")) {
                File(modDir, name).takeIf { it.isFile }?.let {
                    Log.i(TAG, "removing excluded ${it.name}")
                    it.delete()
                }
            }

            val pruned = reconcile(modDir, onLog)
            val files = outcome.strings("files").filterNot { it in pruned }
            val projects = outcome.strings("projects")

            // Recorded rather than returned, because the pass that needs them
            // is a different one: `ModInstaller` runs later and reads the
            // marker. The desktop threads the same two values straight from
            // `setupModrinthModpack` into `downloadMods` in memory.
            LoaderMarker.putModpack(dir, files, projects)

            Pack(loader, mcVersion, loaderVersion, files, projects)
        }
    }

    // -----------------------------------------------------------------------
    // Finding the archive
    // -----------------------------------------------------------------------

    /** Returns the archive URL and the cache key to store it under. */
    private fun resolve(modpack: String): Pair<String, String> {
        val plan = Core.modpackPlan(modpack)
        if (plan["kind"]?.jsonPrimitive?.contentOrNull == "ready") {
            val source = plan["source"]!!.jsonObject
            return source.text("url") to source.text("cacheKey")
        }

        val of = plan["of"]!!.jsonPrimitive.content
        val first = Core.modpackSourceFrom(of, fetchJson(plan.text("url")))
        // A pack with no *featured* release is not a pack with no releases —
        // some only ever publish betas — so the unfiltered list is asked for
        // before giving up.
        val source = first ?: Core.modpackFallbackUrl(modpack)
            ?.let { Core.modpackSourceFrom(of, fetchJson(it)) }
            ?: throw ServerBackendException.Engine(
                "That modpack has no published version to install."
            )

        return source.text("url") to source.text("cacheKey")
    }

    // -----------------------------------------------------------------------
    // Reading the archive
    // -----------------------------------------------------------------------

    private fun readManifest(zip: ZipFile): JsonElement {
        val entry = zip.getEntry(MANIFEST)
            ?: throw ServerBackendException.Engine(
                "That file is not a Modrinth modpack — it has no $MANIFEST."
            )
        return json.parseToJsonElement(zip.getInputStream(entry).use { it.readBytes().decodeToString() })
    }

    /** The mods the manifest names, as the core's `PackFile` shape. */
    private fun manifestMods(manifest: JsonElement): List<JsonObject> =
        manifest.jsonObject["files"]?.jsonArray.orEmpty().mapNotNull { entry ->
            val file = entry.jsonObject
            val path = file["path"]?.jsonPrimitive?.contentOrNull?.replace('\\', '/') ?: return@mapNotNull null
            if (!path.startsWith(MOD_DIR)) return@mapNotNull null
            val url = file["downloads"]?.jsonArray?.firstOrNull()?.jsonPrimitive?.contentOrNull

            buildJsonObject {
                put("filename", path.substringAfterLast('/'))
                file["hashes"]?.jsonObject?.get("sha512")?.jsonPrimitive?.contentOrNull
                    ?.let { put("sha512", it) }
                url?.let { put("url", it) }
                // A project id parsed out of the CDN path, kept as a fallback
                // for the hash lookup — some older CDN URLs carry a version
                // number rather than a base62 id, which is why it is a
                // fallback and not the answer.
                projectIdFromUrl(url)?.let { put("urlProjectId", it) }
                file["fileSize"]?.jsonPrimitive?.longOrNull?.let { put("fileSize", it) }
            }
        }

    /** `cdn.modrinth.com/data/<projectId>/versions/…` */
    private fun projectIdFromUrl(url: String?): String? {
        val marker = "cdn.modrinth.com/data/"
        val start = url?.indexOf(marker)?.takeIf { it >= 0 } ?: return null
        val rest = url.substring(start + marker.length)
        val id = rest.substringBefore('/')
        return id.takeIf { it.isNotEmpty() && rest.contains("/versions/") }
    }

    /**
     * The jars the pack ships inside `overrides/mods/`.
     *
     * Each is hashed and read, because these are the ones Modrinth may not
     * know: a pack's CurseForge builds match no Modrinth file, and the jar's
     * own metadata is the only evidence there is.
     */
    private fun overrideMods(zip: ZipFile): List<JsonObject> =
        zip.entries().asSequence()
            .filter { !it.isDirectory }
            .filter { entry ->
                val name = entry.name.replace('\\', '/')
                name.startsWith(OVERRIDES + MOD_DIR) && name.endsWith(".jar")
            }
            .map { entry ->
                val bytes = zip.getInputStream(entry).use { it.readBytes() }
                buildJsonObject {
                    put("filename", entry.name.substringAfterLast('/'))
                    put("sha512", sha512(bytes))
                    put("facts", readJar(bytes))
                }
            }
            .toList()

    /** What a jar says about itself, read by the core from its own entries. */
    private fun readJar(bytes: ByteArray): JsonObject = runCatching {
        val entries = zipEntries(bytes)
        Core.readModJar(
            fabric = FABRIC_META.firstNotNullOfOrNull { entries[it] },
            tomls = TOML_META.mapNotNull { entries[it] },
        )
    }.getOrElse { buildJsonObject { put("side", "unknown"); put("deps", buildJsonArray {}) } }

    /**
     * The metadata entries of a jar held in memory.
     *
     * `ZipFile` needs a path, and a pack's override jars are already bytes
     * inside another zip — writing each one out just to read two small entries
     * would cost a temporary copy of the whole pack.
     */
    private fun zipEntries(bytes: ByteArray): Map<String, String> {
        val wanted = FABRIC_META + TOML_META
        val out = mutableMapOf<String, String>()
        java.util.zip.ZipInputStream(bytes.inputStream()).use { stream ->
            var entry: ZipEntry? = stream.nextEntry
            while (entry != null) {
                if (entry.name in wanted) out[entry.name] = stream.readBytes().decodeToString()
                entry = stream.nextEntry
            }
        }
        return out
    }

    // -----------------------------------------------------------------------
    // Deciding, and doing
    // -----------------------------------------------------------------------

    private fun decide(
        manifestMods: List<JsonObject>,
        overrideMods: List<JsonObject>,
        env: JsonObject?,
    ): JsonObject {
        val inputs = buildJsonObject {
            put("manifest", JsonArray(manifestMods))
            put("overrides", JsonArray(overrideMods))
            put("excludeFiles", env.text(ENV_EXCLUDE_FILES))
            put("overridesExclusions", env.text(ENV_OVERRIDES_EXCLUSIONS))
        }

        var progress = Core.modpackBegin(inputs)
        var rounds = 0
        while (progress["kind"]?.jsonPrimitive?.contentOrNull != "done") {
            if (++rounds > MAX_ROUNDS) {
                throw ServerBackendException.Engine("The modpack did not resolve.")
            }
            val steps = progress["steps"]?.jsonArray ?: break
            val replies = buildJsonArray {
                for (step in steps) add(perform(step.jsonObject))
            }
            progress = Core.modpackAdvance(progress["state"]!!, replies)
        }
        return progress["outcome"]!!.jsonObject
    }

    /**
     * A failed step is data, not an exception.
     *
     * The core reads a failed hash lookup as "install the pack exactly as its
     * author shipped it" — the fail-safe direction, since without the
     * dependency graph an exclusion could strip a hard dependency.
     */
    private fun perform(step: JsonObject): JsonObject {
        val id = step.text("id")
        return try {
            buildJsonObject {
                put("id", id)
                put("json", fetchJson(step.text("url")))
            }
        } catch (err: Exception) {
            Log.w(TAG, "modpack step $id failed: ${err.message}")
            buildJsonObject {
                put("id", id)
                put("error", err.message ?: "failed")
            }
        }
    }

    /**
     * Copy `overrides/` into the server directory.
     *
     * Two exclusion rules apply here and they are not the same: the core
     * decided which *mod jars* are client-only, and
     * `MODRINTH_OVERRIDES_EXCLUSIONS` applies to **every** override path, not
     * just mods — a pack shipping a client-only resource pack or config is
     * excluded by glob.
     */
    private fun extractOverrides(
        zip: ZipFile,
        dir: File,
        skip: Set<String>,
        exclusions: List<String>,
        onLog: (String) -> Unit,
    ) {
        for (entry in zip.entries()) {
            if (entry.isDirectory) continue
            val name = entry.name.replace('\\', '/')
            if (!name.startsWith(OVERRIDES)) continue
            val relative = name.removePrefix(OVERRIDES)
            if (relative.isEmpty()) continue

            val dest = File(dir, relative)
            // A zip entry naming `../` outside the server directory is a
            // traversal, and the archive came off the internet.
            if (!dest.canonicalPath.startsWith(dir.canonicalPath + File.separator)) {
                Log.w(TAG, "refusing an override that escapes the server directory: $name")
                continue
            }

            val excluded = (exclusions.isNotEmpty() && Core.modpackExcluded(exclusions, relative)) ||
                (relative.startsWith(MOD_DIR) && dest.name in skip)
            if (excluded) {
                // Removed rather than merely skipped: a previous run of this
                // pack may have placed it before the exclusion existed.
                dest.takeIf { it.isFile }?.delete()
                continue
            }

            dest.parentFile?.mkdirs()
            zip.getInputStream(entry).use { input ->
                dest.outputStream().use { input.copyTo(it) }
            }
        }
        onLog("[Homerun] Pack files extracted.")
    }

    private fun fetchManifestMods(outcome: JsonObject, modDir: File, onLog: (String) -> Unit) {
        val wanted = outcome["download"]?.jsonArray.orEmpty()
        if (wanted.isEmpty()) return

        onLog("[Homerun] Downloading ${wanted.size} pack mod${if (wanted.size == 1) "" else "s"}...")
        var done = 0
        for (item in wanted) {
            val file = item.jsonObject
            val dest = File(modDir, file.text("filename"))
            // A pack's file list is fixed for a given version, so a jar that is
            // already here is the right one — there is no newer build to chase.
            if (!dest.isFile) {
                download(file.text("url"), dest)
            }
            done++
            if (done % 25 == 0) onLog("[Homerun] $done of ${wanted.size}...")
        }
    }

    /**
     * Drop rescued client-only mods whose hard dependencies did not survive.
     *
     * Run against the directory as assembled, because that is what the loader
     * reads — and Modrinth's dependency data drifts from what the jars say.
     */
    private fun reconcile(modDir: File, onLog: (String) -> Unit): Set<String> {
        val jars = modDir.listFiles { f -> f.isFile && f.name.endsWith(".jar") } ?: return emptySet()
        val described = buildJsonArray {
            for (jar in jars) {
                add(buildJsonObject {
                    put("filename", jar.name)
                    put("facts", readJar(runCatching { jar.readBytes() }.getOrElse { ByteArray(0) }))
                })
            }
        }

        val pruned = runCatching { Core.modpackReconcile(described) }.getOrDefault(emptyList())
        for (name in pruned) {
            Log.i(TAG, "pruning $name: a hard dependency is not installed server-side")
            File(modDir, name).delete()
        }
        if (pruned.isNotEmpty()) {
            onLog(
                "[Homerun] Removed ${pruned.size} client-only mod${if (pruned.size == 1) "" else "s"} " +
                    "whose dependencies are not installed on a server."
            )
        }
        return pruned.toSet()
    }

    // -----------------------------------------------------------------------
    // Space
    // -----------------------------------------------------------------------

    /**
     * Refuse a pack that will not fit, before fetching any of it.
     *
     * The manifest states every mod's size, so this is arithmetic rather than
     * a guess. Failing here costs a player nothing; failing at 90% costs them
     * a full device, and a phone with no free space misbehaves in ways that
     * have nothing to do with Minecraft.
     */
    private fun requireSpace(dir: File, mods: List<JsonObject>, onLog: (String) -> Unit) {
        val needed = mods.sumOf { it["fileSize"]?.jsonPrimitive?.longOrNull ?: 0L }
        if (needed <= 0) return

        val free = runCatching { dir.usableSpace }.getOrDefault(Long.MAX_VALUE)
        val mb = { bytes: Long -> "${bytes / 1024 / 1024} MB" }
        onLog("[Homerun] The pack's mods need ${mb(needed)}.")

        if (free < needed + FREE_SPACE_MARGIN_BYTES) {
            throw ServerBackendException.Engine(
                "This modpack needs ${mb(needed)} and this device has ${mb(free)} free. " +
                    "Free up some space and try again."
            )
        }
    }

    // -----------------------------------------------------------------------
    // Transfer
    // -----------------------------------------------------------------------

    private fun download(url: String, dest: File) {
        val part = File(dest.parentFile, "${dest.name}.part")
        val connection = open(url)
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            dest.parentFile?.mkdirs()
            connection.inputStream.use { input ->
                part.outputStream().use { input.copyTo(it) }
            }
        } catch (err: Exception) {
            part.delete()
            throw ServerBackendException.Engine(
                "Could not download ${dest.name}: ${err.message ?: "no connection"}"
            )
        } finally {
            connection.disconnect()
        }
        if (!part.renameTo(dest)) {
            part.delete()
            throw ServerBackendException.Engine("Could not move ${dest.name} into place.")
        }
    }

    private fun fetchJson(url: String): JsonElement {
        val connection = open(url).apply { setRequestProperty("Accept", "application/json") }
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            return json.parseToJsonElement(
                connection.inputStream.bufferedReader().use { it.readText() }
            )
        } finally {
            connection.disconnect()
        }
    }

    private fun open(url: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
        }

    private fun sha512(bytes: ByteArray): String =
        MessageDigest.getInstance("SHA-512").digest(bytes)
            .joinToString("") { "%02x".format(it) }

    // -----------------------------------------------------------------------

    private fun exclusions(env: JsonObject?): List<String> =
        env.text(ENV_OVERRIDES_EXCLUSIONS)
            .split('\n', ',')
            .map(String::trim)
            .filter { it.isNotEmpty() }

    private fun JsonObject?.text(key: String): String =
        this?.get(key)?.jsonPrimitive?.contentOrNull.orEmpty()

    private fun JsonObject.strings(key: String): List<String> =
        this[key]?.jsonArray.orEmpty().map { it.jsonPrimitive.content }

    private fun JsonArray?.orEmpty(): List<JsonElement> = this ?: emptyList()

    @Suppress("unused")
    private fun InputStream.drain(): ByteArray = readBytes()

    private const val MAX_ROUNDS = 16

    private const val TAG = "HomerunMods"
}
