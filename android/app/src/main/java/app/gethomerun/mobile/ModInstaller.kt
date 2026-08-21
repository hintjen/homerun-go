package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * Putting the mods a server is configured with onto the device.
 *
 * # What this is, and what the core is
 *
 * Every *decision* is `homerun_core::minecraft::mods` — which version of a mod
 * wins, whether it is client-only and should be skipped, which dependencies it
 * drags in, which jar in the directory is stale enough to delete. This file
 * makes HTTP requests and moves bytes, and it is deliberately incapable of
 * deciding any of the above.
 *
 * The core is pure, and installing mods is not, so the two talk in steps: the
 * core says what to fetch, this fetches it, the core says what that meant.
 *
 * # Why it exists at all
 *
 * Before this, **Android installed no mods and no plugins**. A Paper server
 * configured with plugins on the dashboard started on a phone as bare Paper,
 * silently, while the app advertised `moddedServers: true`. Fabric arrived in
 * M2 with the same gap. This closes it for both.
 *
 * # Parity
 *
 * `downloadMods` in `mod-installer.ts` is the spec, and the desktop carries
 * two hand-maintained copies of it that
 * [`native-mod-support.md`](https://github.com/) says must be fixed in
 * lockstep. This is not a third: the logic lives in Rust and the desktop can
 * adopt it.
 *
 * Two things are this host's own, and both follow `ServerJar`'s precedent:
 * downloads are written to a temporary file and moved into place, and a
 * failure to fetch one mod never fails the launch.
 */
object ModInstaller {

    /** Where `MODRINTH_PROJECTS` and friends live in the server's env. */
    private const val ENV_PROJECTS = "MODRINTH_PROJECTS"
    private const val ENV_EXCLUDED = "EXCLUDED_IDS"

    private const val CONNECT_TIMEOUT_MS = 30_000
    private const val READ_TIMEOUT_MS = 60_000

    private const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

    /**
     * A batch is performed in order rather than in parallel.
     *
     * Modrinth asks for civility and a phone's radio is not helped by fanning
     * out; the batching that matters is already the core's, which asks for a
     * hundred projects in one request rather than one each.
     */
    private val json = Json { ignoreUnknownKeys = true }

    /**
     * Install, update and prune the mods for one server.
     *
     * Never throws for a mod-shaped reason. A server whose mods cannot be
     * resolved still starts — without them — because the alternative is a
     * player unable to play at all because Modrinth was briefly unreachable.
     * The desktop draws the same line. What *is* reported is every mod that
     * did not make it, on the console the player is reading.
     *
     * Blocking and potentially minutes long. Cancellable: a stop mid-install
     * abandons the remaining steps.
     */
    suspend fun sync(
        dir: File,
        loader: String,
        mcVersion: String,
        gameType: String,
        env: JsonObject?,
        onLog: (String) -> Unit,
    ) = withContext(Dispatchers.IO) {
        // A crossplay server's Geyser is not in `MODRINTH_PROJECTS` — nothing
        // ever put it there. It is implied by the game type, so it is folded in
        // here rather than at creation: a server made before this existed then
        // gets it on its next launch, and a bundle cannot be the thing that
        // decides what a crossplay server is.
        val configured = env?.get(ENV_PROJECTS)?.jsonPrimitive?.contentOrNull.orEmpty()
        val listed = runCatching {
            Core.crossplayProjects(gameType = gameType, loader = loader, configured = configured)
        }.getOrElse {
            // This function's contract is that it never fails a launch for a
            // mod-shaped reason, and it is called outside the resolver's own
            // catch. Falling back to what the server configured loses Geyser
            // and starts a Java server, which is the same outcome every other
            // failure here has.
            Log.w(TAG, "could not merge crossplay projects: ${it.message}")
            configured
        }
        val marker = LoaderMarker.read(dir)
        val existing = marker?.get("mods")?.takeIf { it !is JsonNull }?.jsonObject

        // Nothing configured and nothing recorded means there is nothing to
        // install and nothing that could have gone stale, so the whole
        // pipeline is skipped rather than run to produce an empty answer.
        if (listed.isBlank() && existing.isNullOrEmpty()) return@withContext

        val subDir = File(dir, Core.modsSubDir(loader)).apply { mkdirs() }

        val outcome = try {
            resolve(
                inputs = buildJsonObject {
                    put("loader", loader)
                    put("gameVersion", mcVersion)
                    put("projects", listed)
                    put("excluded", env?.get(ENV_EXCLUDED)?.jsonPrimitive?.contentOrNull.orEmpty())
                    existing?.let { put("existing", it) }
                    // Both written by [ModpackInstaller] earlier in this
                    // launch. `modpackFiles` keeps the sweep off a pack's own
                    // jars; `modpackProjects` stops a pack's mod being
                    // installed a second time as somebody's dependency.
                    marker?.get("modpackFiles")?.let { put("modpackFiles", it) }
                    marker?.get("modpackProjects")?.let { put("modpackProjects", it) }
                    put("present", buildJsonArray {
                        subDir.list()?.forEach { add(JsonPrimitive(it)) }
                    })
                },
                subDir = subDir,
                onLog = onLog,
            )
        } catch (err: Exception) {
            // The whole pipeline failed, not one mod. Say so and start the
            // server: whatever is already in `mods/` is what it runs with.
            Log.w(TAG, "mod sync failed: ${err.message}")
            onLog("[Homerun] Could not check for mod updates — starting with what is installed.")
            return@withContext
        }

        apply(outcome, subDir, dir, onLog)
    }

    // -----------------------------------------------------------------------
    // Driving the core
    // -----------------------------------------------------------------------

    /**
     * Run the core's steps until it says it is done, and return the outcome.
     *
     * The loop is bounded. A dependency graph is finite and the core
     * deduplicates it, so the bound is unreachable by design — which is
     * exactly why it is here rather than trusted away: a bug that fails to
     * converge would otherwise hang a server start for ever, and the bridge
     * has no timeout to save it.
     */
    private suspend fun resolve(
        inputs: JsonObject,
        subDir: File,
        onLog: (String) -> Unit,
    ): JsonObject {
        var progress = Core.modsBegin(inputs)
        var rounds = 0

        while (progress["kind"]?.jsonPrimitive?.contentOrNull != "done") {
            if (++rounds > MAX_ROUNDS) {
                throw IllegalStateException("mod resolution did not settle in $MAX_ROUNDS rounds")
            }
            val steps = progress["steps"]?.jsonArray ?: break
            val replies = buildJsonArray {
                for (step in steps) add(perform(step.jsonObject, subDir, onLog))
            }
            progress = Core.modsAdvance(progress["state"]!!, replies)
        }

        return progress["outcome"]!!.jsonObject
    }

    /**
     * Do one step, and report what happened rather than throwing.
     *
     * A failed step is data the core knows what to do with — a mod that cannot
     * be resolved is preserved rather than deleted, a `server_side` lookup that
     * fails makes the whole exclusion pass fail open. Throwing here would
     * discard the run instead.
     */
    private fun perform(step: JsonObject, subDir: File, onLog: (String) -> Unit): JsonObject {
        val id = step["id"]!!.jsonPrimitive.content
        return try {
            when (step["kind"]?.jsonPrimitive?.contentOrNull) {
                "json" -> reply(id, fetchJson(step["url"]!!.jsonPrimitive.content))
                "download" -> {
                    val filename = step["filename"]!!.jsonPrimitive.content
                    onLog("[Homerun] Downloading $filename...")
                    download(step["url"]!!.jsonPrimitive.content, File(subDir, filename))
                    reply(id, null)
                }
                else -> failure(id, "unknown step")
            }
        } catch (err: Exception) {
            Log.w(TAG, "step $id failed: ${err.message}")
            failure(id, err.message ?: "failed")
        }
    }

    private fun reply(id: String, body: JsonElement?) = buildJsonObject {
        put("id", id)
        body?.let { put("json", it) }
    }

    private fun failure(id: String, why: String) = buildJsonObject {
        put("id", id)
        put("error", why)
    }

    // -----------------------------------------------------------------------
    // Applying what the core decided
    // -----------------------------------------------------------------------

    private fun apply(outcome: JsonObject, subDir: File, dir: File, onLog: (String) -> Unit) {
        // Deleting is the core's call and it is a narrow one: only files a
        // previous run installed are ever candidates, so a jar the player
        // added by hand is never touched. See `mods::sweep`.
        for (name in outcome["remove"]?.jsonArray.orEmpty()) {
            val stale = File(subDir, name.jsonPrimitive.content)
            Log.i(TAG, "removing stale ${stale.name}")
            stale.delete()
        }

        val installed = outcome["installed"]?.jsonArray.orEmpty()
        if (installed.isNotEmpty()) {
            onLog("[Homerun] ${installed.size} mod${if (installed.size == 1) "" else "s"} ready.")
        }

        // Named individually, because "3 mods failed" is not something a
        // player can act on and "create is not available for 1.21.4" is.
        for (entry in outcome["failed"]?.jsonArray.orEmpty()) {
            val failed = entry.jsonObject
            val slug = failed["slug"]?.jsonPrimitive?.contentOrNull ?: continue
            onLog("[Homerun] ${explain(slug, failed["reason"]?.jsonPrimitive?.contentOrNull)}")
        }

        outcome["records"]?.let { LoaderMarker.putMods(dir, it) }
    }

    /** The reasons, in words a player can do something with. */
    private fun explain(slug: String, reason: String?): String = when (reason) {
        "no_release_version" -> "$slug has no published version, so it was skipped."
        "incompatible" -> "$slug has no build for this Minecraft version, so it was skipped."
        else -> "$slug could not be downloaded, so it was skipped."
    }

    // -----------------------------------------------------------------------
    // Transfer
    // -----------------------------------------------------------------------

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

    /**
     * Fetch to a temporary file and move it into place.
     *
     * A half-written jar with the right name is worse than no jar: the loader
     * reads it, fails, and the record beside it says it is fine — so the next
     * start does not re-fetch it. `ServerJar` takes the same precaution for
     * the same reason.
     */
    private fun download(url: String, dest: File) {
        val part = File(dest.parentFile, "${dest.name}.part")
        val connection = open(url)
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            connection.inputStream.use { input ->
                part.outputStream().use { input.copyTo(it) }
            }
        } catch (err: Exception) {
            part.delete()
            throw err
        } finally {
            connection.disconnect()
        }
        if (!part.renameTo(dest)) {
            part.delete()
            throw IOException("could not move ${dest.name} into place")
        }
    }

    private fun open(url: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
        }

    private fun JsonArray?.orEmpty(): List<JsonElement> = this ?: emptyList()

    /**
     * Generous, and unreachable unless the core has a bug: each round is one
     * level of the dependency graph, and real packs are a handful deep.
     */
    private const val MAX_ROUNDS = 64

    private const val TAG = "HomerunMods"
}
