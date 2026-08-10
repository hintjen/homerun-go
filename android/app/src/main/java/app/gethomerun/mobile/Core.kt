package app.gethomerun.mobile

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.add
import kotlinx.serialization.json.boolean
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonArray
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
        call("minecraft.jar.resolveVersion", buildJsonObject {
            put("manifest", manifest)
            requested?.let { put("version", it) }
        }).jsonPrimitive.content

    fun metadataUrl(manifest: JsonElement, version: String): String =
        call("minecraft.jar.metadataUrl", buildJsonObject {
            put("manifest", manifest)
            put("version", version)
        }).jsonPrimitive.content

    fun vanillaArtifact(metadata: JsonElement, version: String): JsonObject =
        call("minecraft.jar.vanilla", buildJsonObject {
            put("metadata", metadata)
            put("version", version)
        }).jsonObject

    fun paperArtifact(builds: JsonElement, version: String, requiredJava: Int): JsonObject =
        call("minecraft.jar.paper", buildJsonObject {
            put("builds", builds)
            put("version", version)
            put("requiredJava", requiredJava)
        }).jsonObject

    /** `vanilla` or `paper`; anything needing an installer throws by name. */
    fun parseLoader(type: String?): String =
        call("minecraft.jar.parseLoader", buildJsonObject {
            type?.let { put("type", it) }
        }).jsonPrimitive.content

    /** Throws with a sentence for the player if the bundled runtime is too old. */
    fun checkJava(artifact: JsonObject, bundledJava: Int?) {
        call("minecraft.jar.checkJava", buildJsonObject {
            put("artifact", artifact)
            bundledJava?.let { put("bundledJava", it) }
        })
    }

    fun jarSatisfies(onDisk: JsonObject, artifact: JsonObject): Boolean =
        call("minecraft.jar.satisfies", buildJsonObject {
            put("onDisk", onDisk)
            put("artifact", artifact)
        }).jsonPrimitive.boolean

    fun jarCouldSatisfy(onDisk: JsonObject, version: String?, loader: String): Boolean =
        call("minecraft.jar.couldSatisfy", buildJsonObject {
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
    ): String = call("tunnel.render", buildJsonObject {
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
    // Config, through the game capability surface
    // -----------------------------------------------------------------------
    //
    // Nothing below names Minecraft except the default game id. The host asks
    // which files to read, which identities it must fetch, and what to write —
    // and the core answers for whichever game it was asked about. That is what
    // stops `server.properties` knowledge, latin-1 knowledge and UUID
    // derivation leaking back out into three platforms.

    /** The game this app hosts today. The only place it is named. */
    const val MINECRAFT = "minecraft-java"

    /** How a config file must be read and written. */
    enum class Encoding {
        UTF8, LATIN1;

        val charset: java.nio.charset.Charset
            get() = when (this) {
                UTF8 -> Charsets.UTF_8
                // `§` — the colour-code marker in a MOTD — is one byte here
                // and two in UTF-8. Reading or writing with the wrong one
                // destroys it, on a launch that changed nothing.
                LATIN1 -> Charsets.ISO_8859_1
            }

        companion object {
            fun parse(raw: String?): Encoding = if (raw == "latin1") LATIN1 else UTF8
        }
    }

    /** A file the game needs read before it can decide what to write. */
    data class ConfigInput(val path: String, val encoding: Encoding)

    /** A file to write before the server starts. */
    data class ConfigFile(val path: String, val contents: String, val encoding: Encoding)

    /** A player resolved to the identity the server will match them by. */
    data class Identity(val name: String, val id: String)

    /** Which files to read, and how to decode each. */
    fun configInputs(env: JsonObject, game: String = MINECRAFT): List<ConfigInput> =
        call("game.configInputs", buildJsonObject {
            put("game", game)
            put("env", env)
        }).jsonArray.map {
            val entry = it.jsonObject
            ConfigInput(
                path = entry["path"]!!.jsonPrimitive.content,
                encoding = Encoding.parse(entry["encoding"]?.jsonPrimitive?.contentOrNull),
            )
        }

    /**
     * Names the host must resolve over the network.
     *
     * Only what the game *cannot* derive itself: an offline Minecraft server
     * returns nothing here and costs no requests, because its UUIDs are a
     * function of the name and the core derives them internally.
     */
    fun requiredLookups(
        env: JsonObject,
        gameType: String,
        game: String = MINECRAFT,
    ): List<String> =
        call("game.requiredLookups", buildJsonObject {
            put("game", game)
            put("env", env)
            put("gameType", gameType)
        }).jsonArray.map { it.jsonObject["name"]!!.jsonPrimitive.content }

    /**
     * The files to write, given everything the host gathered.
     *
     * [existing] is keyed by the paths [configInputs] named; a file that does
     * not exist is simply left out. [resolved] carries whatever identities the
     * host managed to fetch — a name missing from it is the game's to handle,
     * and Minecraft skips it rather than writing an id that cannot match.
     */
    fun configFiles(
        env: JsonObject,
        gameType: String,
        port: Int,
        bindAddress: String,
        existing: Map<String, String>,
        resolved: List<Identity>,
        now: String,
        game: String = MINECRAFT,
    ): List<ConfigFile> =
        call("game.configFiles", buildJsonObject {
            put("game", game)
            put("context", buildJsonObject {
                put("env", env)
                put("game_type", gameType)
                put("port", port)
                put("bind_address", bindAddress)
                put("existing", buildJsonObject {
                    existing.forEach { (path, contents) -> put(path, contents) }
                })
                put("resolved", buildJsonArray {
                    resolved.forEach {
                        add(buildJsonObject { put("name", it.name); put("id", it.id) })
                    }
                })
                put("now", now)
            })
        }).jsonArray.map {
            val entry = it.jsonObject
            ConfigFile(
                path = entry["path"]!!.jsonPrimitive.content,
                contents = entry["contents"]!!.jsonPrimitive.content,
                encoding = Encoding.parse(entry["encoding"]?.jsonPrimitive?.contentOrNull),
            )
        }

    /**
     * Dash Mojang's 32-character hex id.
     *
     * The one game-specific call the host still makes, because fetching the
     * profile is the host's job and the response shape comes with it. Throws
     * if the value is not a 32-character hex id.
     */
    fun dashUuid(undashed: String): String =
        call("minecraft.settings.dashUuid", buildJsonObject { put("undashed", undashed) })
            .jsonPrimitive.content

    // -----------------------------------------------------------------------
    // Console
    // -----------------------------------------------------------------------

    /** What one console line means, if anything. */
    data class Line(val ready: Boolean, val joined: String?, val left: String?)

    fun classify(line: String): Line {
        val reply = call("game.classify", buildJsonObject { put("line", line) }).jsonObject
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
