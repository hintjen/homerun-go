package app.gethomerun.mobile

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.boolean
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put

/**
 * The decisions this app shares with the desktop and iOS, in `homerun-core`.
 *
 * # Why these live in Rust
 *
 * Everything reachable through here was, until recently, written twice — once
 * in the desktop's TypeScript and once in this app's Kotlin, both from the
 * same reference. They had already drifted: the desktop installs Paper's
 * oldest build where this app installs the newest stable, because the two
 * implementations read the same array in opposite directions.
 *
 * So the *decisions* moved to one tested place and the platforms kept what
 * only they can do. This app still makes every HTTP request, spawns every
 * process and owns every file; it just stops deciding what any of it means.
 *
 * # The shape
 *
 * One native entry point, JSON in and out, replying `{ok, value}` or
 * `{ok:false, error}`. A dozen mangled symbols would save microseconds and
 * cost a dozen places for two languages to disagree about argument order.
 *
 * Failures are [CoreException], and they are *verdicts* — "this loader needs
 * an installer", "that version does not exist" — carrying text meant for a
 * player, not a stack trace.
 */
object Core {

    init {
        // Same library as the engine. Loading it here too is harmless (the VM
        // dedupes) and means Core works whether or not a server has ever run.
        System.loadLibrary("homerun_pumpkin_ffi")
    }

    private external fun nativeCall(method: String, args: String): String?

    private val json = Json { ignoreUnknownKeys = true; explicitNulls = false }

    class CoreException(message: String) : Exception(message)

    /**
     * Call into the core.
     *
     * @throws CoreException when the core says no, with its wording intact.
     */
    fun call(method: String, args: JsonObject = JsonObject(emptyMap())): JsonElement {
        val raw = nativeCall(method, args.toString())
            ?: throw CoreException("The native core did not answer.")
        val reply = runCatching { json.parseToJsonElement(raw).jsonObject }
            .getOrElse { throw CoreException("The native core answered with nonsense.") }

        if (reply["ok"]?.jsonPrimitive?.boolean != true) {
            throw CoreException(
                reply["error"]?.jsonPrimitive?.contentOrNull ?: "The native core refused."
            )
        }
        return reply["value"] ?: JsonNull
    }

    // -----------------------------------------------------------------------
    // Jars
    // -----------------------------------------------------------------------

    /** Turn an absent, blank or `LATEST` version into a concrete release. */
    fun resolveVersion(manifest: JsonElement, requested: String?): String =
        call("jar.resolveVersion", buildJsonObject {
            put("manifest", manifest)
            requested?.let { put("version", it) }
        }).jsonPrimitive.content

    fun metadataUrl(manifest: JsonElement, version: String): String =
        call("jar.metadataUrl", buildJsonObject {
            put("manifest", manifest)
            put("version", version)
        }).jsonPrimitive.content

    fun vanillaArtifact(metadata: JsonElement, version: String): JsonObject =
        call("jar.vanilla", buildJsonObject {
            put("metadata", metadata)
            put("version", version)
        }).jsonObject

    fun paperArtifact(builds: JsonElement, version: String, requiredJava: Int): JsonObject =
        call("jar.paper", buildJsonObject {
            put("builds", builds)
            put("version", version)
            put("requiredJava", requiredJava)
        }).jsonObject

    /** `vanilla` or `paper`; anything needing an installer throws by name. */
    fun parseLoader(type: String?): String =
        call("jar.parseLoader", buildJsonObject {
            type?.let { put("type", it) }
        }).jsonPrimitive.content

    /** Throws with a sentence for the player if the bundled runtime is too old. */
    fun checkJava(artifact: JsonObject, bundledJava: Int?) {
        call("jar.checkJava", buildJsonObject {
            put("artifact", artifact)
            bundledJava?.let { put("bundledJava", it) }
        })
    }

    fun jarSatisfies(onDisk: JsonObject, artifact: JsonObject): Boolean =
        call("jar.satisfies", buildJsonObject {
            put("onDisk", onDisk)
            put("artifact", artifact)
        }).jsonPrimitive.boolean

    fun jarCouldSatisfy(onDisk: JsonObject, version: String?, loader: String): Boolean =
        call("jar.couldSatisfy", buildJsonObject {
            put("onDisk", onDisk)
            version?.let { put("version", it) }
            put("loader", loader)
        }).jsonPrimitive.boolean

    // -----------------------------------------------------------------------
    // The tunnel
    // -----------------------------------------------------------------------

    /**
     * Render the wireproxy config.
     *
     * Byte-exact against the desktop's generator, and tested that way — the
     * gateway is the same on both sides, so a divergence is a bug by
     * definition.
     */
    fun renderWireproxy(
        link: JsonObject,
        port: Int,
        exposure: String = "java",
        geyserPort: Int? = null,
        voiceChatPort: Int? = null,
    ): String = call("wireproxy.render", buildJsonObject {
        put("link", link)
        put("port", port)
        put("exposure", exposure)
        geyserPort?.let { put("geyserPort", it) }
        voiceChatPort?.let { put("voiceChatPort", it) }
    }).jsonPrimitive.content

    /** The tunnel on a `/api/server/<id>/` body, or null if none yet. */
    fun linkFromServerBody(body: JsonElement): JsonObject? =
        call("link.fromServerBody", buildJsonObject { put("body", body) })
            .let { if (it is JsonNull) null else it.jsonObject }

    /** False when these are the dead credentials from the previous session. */
    fun linkIsUsable(polled: JsonObject, before: JsonObject?): Boolean =
        call("link.isUsable", buildJsonObject {
            put("polled", polled)
            before?.let { put("before", it) }
        }).jsonPrimitive.boolean

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /** `stopped` or `crashed`. A server exits 0 on `stop`, so intent decides. */
    fun exitState(intentional: Boolean, code: Int): String =
        call("state.exit", buildJsonObject {
            put("intentional", intentional)
            put("code", code)
        }).jsonPrimitive.content

    /**
     * One line of wireproxy output against a running count.
     *
     * [watch] is opaque state, held by the caller and handed back each line —
     * so there is no native allocation to remember to free, and the threshold
     * and its reset rule stay in one place.
     */
    data class Handshake(val watch: JsonObject, val giveUp: Boolean, val recovered: Boolean)

    fun observeHandshake(watch: JsonObject?, line: String): Handshake {
        val reply = call("state.handshake", buildJsonObject {
            watch?.let { put("watch", it) }
            put("line", line)
        }).jsonObject
        return Handshake(
            watch = reply["watch"]!!.jsonObject,
            giveUp = reply["giveUp"]?.jsonPrimitive?.boolean == true,
            recovered = reply["recovered"]?.jsonPrimitive?.boolean == true,
        )
    }

    // -----------------------------------------------------------------------
    // Console
    // -----------------------------------------------------------------------

    /** What one console line means, if anything. */
    data class Line(val ready: Boolean, val joined: String?, val left: String?)

    fun classify(line: String): Line {
        val reply = call("console.classify", buildJsonObject { put("line", line) }).jsonObject
        fun name(key: String) = reply[key]?.let {
            (it as? JsonPrimitive)?.contentOrNull
        }
        return Line(
            ready = reply["ready"]?.jsonPrimitive?.boolean == true,
            joined = name("joined"),
            left = name("left"),
        )
    }
}
