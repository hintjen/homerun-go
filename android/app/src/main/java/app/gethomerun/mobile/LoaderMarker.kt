package app.gethomerun.mobile

import android.util.Log
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import java.io.File

/**
 * `.homerun-loader.json` — what a server directory remembers about itself.
 *
 * The desktop's name and shape, deliberately, so a directory restored from a
 * desktop backup is understood rather than reinstalled. It records the loader,
 * the Minecraft version it was installed for, any pinned loader build, and the
 * mods installed into it.
 *
 * # Why it is one file with two writers
 *
 * [ServerLoader] owns the loader fields and [ModInstaller] owns `mods`, and
 * they run at different times — the desktop's `setupServerLoader` writes the
 * marker before `downloadMods` has resolved anything. So **every write is a
 * merge**: each writer replaces the keys it owns and preserves everything
 * else. Round-tripping through a narrower type would silently drop a mod
 * record on the first restart after a restore, which is the exact bug the
 * desktop's `writeLoaderMeta` preserves `mods` and `modpackFiles` to avoid.
 *
 * # When it is thrown away
 *
 * `Core.loaderFilesToClean` includes it, so a loader or version change wipes
 * it along with the install. That is intended: mod records describe files that
 * no longer exist once the loader has been torn down.
 */
object LoaderMarker {

    private const val NAME = ".homerun-loader.json"

    private val json = Json { ignoreUnknownKeys = true }

    /** The marker as written, or null when there is not one to read. */
    fun read(dir: File): JsonObject? = runCatching {
        json.parseToJsonElement(File(dir, NAME).readText()) as? JsonObject
    }.getOrNull()

    /**
     * Record the installed loader, preserving `mods` and anything else.
     *
     * Created when absent: this is the only place a **downloaded-jar** server
     * (vanilla, Paper) gets a marker at all, because it never runs an
     * installer — and its mod records still need somewhere to live. The
     * desktop reaches the same state by running `setupServerLoader` for every
     * loader including vanilla.
     */
    fun putLoader(dir: File, loader: String, mcVersion: String, loaderVersion: String?) {
        merge(dir, buildMap {
            put("loader", JsonPrimitive(loader))
            put("mcVersion", JsonPrimitive(mcVersion))
            loaderVersion?.let { put("loaderVersion", JsonPrimitive(it)) }
        })
    }

    /** Record the installed mods, preserving the loader fields. */
    fun putMods(dir: File, mods: JsonElement) {
        merge(dir, mapOf("mods" to mods))
    }

    /**
     * Record what a modpack placed.
     *
     * `modpackFiles` is the desktop's, and the stale sweep reads it to know
     * which jars a pack owns rather than a mod list. `modpackProjects` is this
     * host's addition: the desktop threads it in memory from
     * `setupModrinthModpack` straight into `downloadMods`, and here the two run
     * as separate passes, so it has to survive in between — otherwise a pack's
     * own mod is installed a second time under a `dep:` key and the server
     * gets two copies of it.
     */
    fun putModpack(dir: File, files: List<String>, projects: List<String>) {
        merge(dir, mapOf(
            "modpackFiles" to JsonArray(files.map(::JsonPrimitive)),
            "modpackProjects" to JsonArray(projects.map(::JsonPrimitive)),
        ))
    }

    private fun merge(dir: File, fields: Map<String, JsonElement>) {
        val merged = buildMap {
            read(dir)?.forEach { (key, value) -> put(key, value) }
            putAll(fields)
        }
        runCatching { File(dir, NAME).writeText(JsonObject(merged).toString()) }
            .onFailure { Log.w(TAG, "could not write $NAME: ${it.message}") }
    }

    private const val TAG = "HomerunJava"
}
