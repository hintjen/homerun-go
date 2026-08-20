package app.gethomerun.mobile

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.add
import kotlinx.serialization.json.boolean
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.long
import kotlinx.serialization.json.longOrNull
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

    /**
     * Which staged runtime should run this jar.
     *
     * The lowest of [bundled] that satisfies the jar, not the newest — the core
     * explains why, and it is the rule the two-runtime build turns on. Throws
     * with a sentence for the player when none of them will do.
     */
    fun selectRuntime(artifact: JsonObject, loader: String, bundled: List<Int>): Int =
        call("minecraft.jar.selectRuntime", buildJsonObject {
            put("artifact", artifact)
            put("loader", loader)
            put("bundled", buildJsonArray { bundled.forEach { add(it) } })
        }).jsonPrimitive.int

    /**
     * [selectRuntime] for a Java requirement with no artifact behind it.
     *
     * The bundler check is the caller: a server jar can need a newer Java than
     * Mojang's manifest claimed, and by the time that is known the loader's
     * installer has produced the jar and there is no artifact left to consult.
     * [what] becomes the subject of the refusal a player reads.
     */
    fun selectRuntimeFor(
        requiredJava: Int,
        what: String,
        loader: String,
        bundled: List<Int>,
    ): Int = call("minecraft.jar.selectRuntimeFor", buildJsonObject {
        put("requiredJava", requiredJava)
        put("what", what)
        put("loader", loader)
        put("bundled", buildJsonArray { bundled.forEach { add(it) } })
    }).jsonPrimitive.int

    fun jarSatisfies(onDisk: JsonObject, artifact: JsonObject): Boolean =
        call("minecraft.jar.satisfies", buildJsonObject {
            put("onDisk", onDisk)
            put("artifact", artifact)
        }).jsonPrimitive.boolean

    /** What the core decided about the jar already in a server's directory. */
    data class Cached(
        /** `use`, `verify`, `adopt` or `download`. */
        val action: String,
        /** Only on `verify`: `Sha1` or `Sha256`, in the core's spelling. */
        val algorithm: String?,
    )

    /**
     * Whether the jar on disk can be kept.
     *
     * Two steps, because the answer sometimes needs a digest and hashing tens
     * of megabytes to settle a question a marker file usually answers is the
     * wrong default. Call with [digest] null; if the reply is `verify`, hash
     * the file with the algorithm it names and call again.
     */
    fun jarCacheDecision(
        onDisk: JsonObject?,
        present: Boolean,
        digest: String?,
        artifact: JsonObject,
    ): Cached = call("minecraft.jar.cacheDecision", buildJsonObject {
        onDisk?.let { put("onDisk", it) }
        put("present", present)
        digest?.let { put("digest", it) }
        put("artifact", artifact)
    }).jsonObject.let {
        Cached(
            action = it["action"]?.jsonPrimitive?.contentOrNull.orEmpty(),
            algorithm = it["algorithm"]?.jsonPrimitive?.contentOrNull,
        )
    }

    /**
     * What to call this jar in the shared cache, or null if it cannot be
     * cached. Content-addressed, so two servers on one version name one file.
     */
    fun jarCacheKey(artifact: JsonObject): String? =
        call("minecraft.jar.cacheKey", buildJsonObject {
            put("artifact", artifact)
        }).jsonPrimitive.contentOrNull

    /**
     * How long to wait before retrying a download, or null when there is
     * nothing left to try.
     *
     * Null is what ends the loop. Treating a missing delay as zero would
     * retry a dead endpoint as fast as it can refuse.
     */
    fun jarRetryDelayMs(attempt: Int): Long? =
        call("minecraft.jar.retryDelay", buildJsonObject { put("attempt", attempt) })
            .jsonPrimitive.longOrNull
    fun jarCouldSatisfy(onDisk: JsonObject, version: String?, loader: String): Boolean =
        call("minecraft.jar.couldSatisfy", buildJsonObject {
            put("onDisk", onDisk)
            version?.let { put("version", it) }
            put("loader", loader)
        }).jsonPrimitive.boolean

    // -----------------------------------------------------------------------
    // Loaders that install by running an installer
    //
    // Vanilla and Paper publish a server jar; Fabric publishes an installer
    // that is run once and leaves a launchable server behind. The two share a
    // version resolver and nothing else, so the host takes one path or the
    // other and [loaderIsInstalled] is the question that decides which.
    // -----------------------------------------------------------------------

    /** True when this loader is installed by running something, not downloading it. */
    fun loaderIsInstalled(loader: String): Boolean =
        call("minecraft.loader.isInstalled", buildJsonObject {
            put("loader", loader)
        }).jsonPrimitive.boolean

    /** The jar to launch once the loader is installed, or null for a plain server jar. */
    fun loaderLaunchJar(loader: String): String? =
        call("minecraft.loader.launchJar", buildJsonObject {
            put("loader", loader)
        }).jsonPrimitive.contentOrNull

    /**
     * Fabric's installer index, to fetch and hand back to [fabricInstallerUrl].
     *
     * The endpoint comes from the core so there is no second copy to drift.
     * `ServerJar`'s Mojang and PaperMC URLs predate that and are still spelled
     * on both sides.
     */
    val FABRIC_INSTALLER_META: String
        get() = call("minecraft.loader.fabricInstallerMeta").jsonPrimitive.content

    /** Quilt's installer index, to fetch and hand back to [quiltInstallerUrl]. */
    val QUILT_INSTALLER_META: String
        get() = call("minecraft.loader.quiltInstallerMeta").jsonPrimitive.content

    /** Where a versioned loader publishes its builds. */
    fun loaderMetadataUrl(loader: String): String = call(
        when (loader) {
            "neoforge" -> "minecraft.loader.neoforgeMetadata"
            else -> "minecraft.loader.forgeMetadata"
        }
    ).jsonPrimitive.content

    /**
     * The loader build to install, from its maven metadata.
     *
     * A [pinned] build is honoured when the metadata still has it; otherwise
     * the newest is used, because a pin that has been deleted upstream should
     * not stop a server starting.
     */
    fun resolveLoaderVersion(
        loader: String,
        metadata: String,
        mcVersion: String,
        pinned: String?,
    ): String = call("minecraft.loader.resolveVersion", buildJsonObject {
        put("loader", loader)
        put("metadata", metadata)
        put("mcVersion", mcVersion)
        pinned?.let { put("pinned", it) }
    }).jsonPrimitive.content

    /** Where to fetch a resolved loader build's installer. */
    fun loaderInstallerUrl(loader: String, version: String): String =
        call("minecraft.loader.installerUrl", buildJsonObject {
            put("loader", loader)
            put("version", version)
        }).jsonPrimitive.content

    /** Which installer to download, from Fabric's installer index. */
    fun fabricInstallerUrl(meta: JsonElement): String =
        call("minecraft.loader.fabricInstallerUrl", buildJsonObject {
            put("meta", meta)
        }).jsonPrimitive.content

    /**
     * Which installer to download, from Quilt's installer index.
     *
     * Not [fabricInstallerUrl] against a different URL: Quilt's index marks no
     * entry stable, so the rule for picking from it is genuinely a different
     * rule. Keeping them separate is what stops Fabric's "first stable, else
     * first" quietly collapsing into "first" and looking deliberate.
     */
    fun quiltInstallerUrl(meta: JsonElement): String =
        call("minecraft.loader.quiltInstallerUrl", buildJsonObject {
            put("meta", meta)
        }).jsonPrimitive.content

    /** Where Quilt says whether it has mapped a Minecraft version at all. */
    fun quiltIntermediaryUrl(mcVersion: String): String =
        call("minecraft.loader.quiltIntermediaryUrl", buildJsonObject {
            put("mcVersion", mcVersion)
        }).jsonPrimitive.content

    /**
     * Throws when Quilt has published no mappings for [mcVersion].
     *
     * Asked before the installer runs, because Quilt trails Minecraft by weeks
     * and its installer does not fail in a way anyone could act on.
     */
    fun ensureQuiltSupports(mcVersion: String, intermediary: JsonElement) {
        call("minecraft.loader.ensureQuiltSupports", buildJsonObject {
            put("mcVersion", mcVersion)
            put("intermediary", intermediary)
        })
    }

    /**
     * Whether what is installed has to be torn down and installed again.
     *
     * A null or unreadable [installed] means yes, which is the safe direction:
     * a marker we cannot trust is one that cannot vouch for what is on disk.
     */
    fun loaderNeedsReinstall(
        installed: JsonObject?,
        loader: String,
        mcVersion: String,
        loaderVersion: String?,
    ): Boolean = call("minecraft.loader.needsReinstall", buildJsonObject {
        installed?.let { put("installed", it) }
        put("loader", loader)
        put("mcVersion", mcVersion)
        loaderVersion?.let { put("loaderVersion", it) }
    }).jsonPrimitive.boolean

    /**
     * What to delete before installing a loader, given the directory listing.
     *
     * Includes files belonging to loaders this build cannot host: a server
     * directory restored from a desktop backup can carry a Forge install, and
     * switching it to Fabric has to clear those jars or the next start finds
     * two servers to run.
     */
    fun loaderFilesToClean(entries: List<String>): List<String> =
        call("minecraft.loader.filesToClean", buildJsonObject {
            put("entries", buildJsonArray { entries.forEach { add(it) } })
        }).jsonArray.map { it.jsonPrimitive.content }

    /**
     * The Java major a server jar's bundler needs, from the first eight bytes
     * of `net/minecraft/bundler/Main.class`.
     *
     * Mojang's manifest states a version and the jar can disagree; the jar
     * wins, because it is what fails. Null when those bytes are not a class
     * file, which is not an error — plenty of jars have no bundler.
     */
    fun bundlerJavaMajor(head: ByteArray): Int? =
        call("minecraft.loader.bundlerJavaMajor", buildJsonObject {
            put("head", buildJsonArray { head.forEach { add(it.toInt() and 0xFF) } })
        }).jsonPrimitive.intOrNull

    // -----------------------------------------------------------------------
    // Argfiles
    //
    // Forge and NeoForge launch entirely through `@argfile`s, and expanding
    // one is a feature of the `java` launcher binary — which this platform
    // does not have, because the VM is created through JNI. See
    // `minecraft::argfile`, and `docs/android-server-backend.md`.
    // -----------------------------------------------------------------------

    /** A launch, split the way `JNI_CreateJavaVM` needs it. */
    data class Expanded(
        val jvmOptions: List<String>,
        val mainClass: String?,
        val programArgs: List<String>,
    )

    /**
     * Expand argfiles, in the order the run script names them.
     *
     * Not a passthrough: the `java` launcher rewrites what it forwards, so
     * `-p <path>` becomes `--module-path=<path>` here. The VM accepts only the
     * joined form and answers the other with `Unrecognized option`.
     */
    fun expandArgfiles(contents: List<String>): Expanded {
        val reply = call("minecraft.argfile.expand", buildJsonObject {
            put("contents", buildJsonArray { contents.forEach { add(it) } })
        }).jsonObject
        return Expanded(
            jvmOptions = reply["jvmOptions"]!!.jsonArray.map { it.jsonPrimitive.content },
            mainClass = reply["mainClass"]?.jsonPrimitive?.contentOrNull,
            programArgs = reply["programArgs"]!!.jsonArray.map { it.jsonPrimitive.content },
        )
    }

    /** The argfiles a run script names, relative to the server directory. */
    fun referencedArgfiles(runScript: String): List<String> =
        call("minecraft.argfile.referenced", buildJsonObject {
            put("runScript", runScript)
        }).jsonArray.map { it.jsonPrimitive.content }

    /** Which of the generated run scripts to believe. `run.sh`, if there is one. */
    fun preferredRunScript(present: List<String>): String? =
        call("minecraft.argfile.runScript", buildJsonObject {
            put("present", buildJsonArray { present.forEach { add(it) } })
        }).jsonPrimitive.contentOrNull

    /** The argfile to use when no run script names one. */
    fun fallbackArgfile(paths: List<String>): String? =
        call("minecraft.argfile.fallback", buildJsonObject {
            put("paths", buildJsonArray { paths.forEach { add(it) } })
        }).jsonPrimitive.contentOrNull

    // -----------------------------------------------------------------------
    // Which mods a server gets
    //
    // A driver rather than a function, because installing mods is three phases
    // of interleaved HTTP with a graph search in the middle and the core has
    // no I/O. [modsBegin] says what to fetch; [modsAdvance] says what the
    // answers meant and what to fetch next. Every decision — which version
    // wins, what is skipped, which dependency is pulled in, which jar is
    // swept — is the core's. See `minecraft::mods`.
    // -----------------------------------------------------------------------

    /** Start resolving. [inputs] is `mods::Inputs`. */
    fun modsBegin(inputs: JsonObject): JsonObject =
        call("minecraft.mods.begin", buildJsonObject { put("inputs", inputs) }).jsonObject

    /** Report what the last batch of steps did, and get the next one. */
    fun modsAdvance(state: JsonElement, replies: JsonArray): JsonObject =
        call("minecraft.mods.advance", buildJsonObject {
            put("state", state)
            put("replies", replies)
        }).jsonObject

    /** `mods` or `plugins`, by loader. */
    fun modsSubDir(loader: String): String =
        call("minecraft.mods.subDir", buildJsonObject {
            put("loader", loader)
        }).jsonPrimitive.content

    // -----------------------------------------------------------------------
    // Minigames
    //
    // A minigame server is a public Paper server built from a template in the
    // Games browser. Three things in its env make it one, and all three are
    // read here rather than decided by this host: which jars to fetch, whether
    // it is a minigame at all, and which of its settings its plugins are
    // allowed to see. See `minecraft::minigame` — a different module from
    // `reporting::minigame`, which reads a finished match off the console and
    // is wired in [Reporting].
    // -----------------------------------------------------------------------

    /** One Homerun-hosted plugin jar: where to get it, what to call it. */
    data class CustomPlugin(val url: String, val filename: String)

    /**
     * Is this a minigame server?
     *
     * The answer decides three things a host does differently for one: no
     * world is restored, no backup is pushed, and the directory is deleted
     * once it stops. A lobby is generated for a single session and nobody's
     * building is in it.
     */
    fun isMinigame(env: JsonObject?): Boolean =
        call("minecraft.minigame.isMinigame", buildJsonObject {
            put("env", env ?: JsonNull)
        }).jsonPrimitive.boolean

    /**
     * The plugin jars this server needs, in catalog order.
     *
     * Ours, not Modrinth's, so [ModInstaller] will never fetch them. Empty for
     * a loader that would not load a jar out of `plugins/` anyway.
     */
    fun customPlugins(loader: String, env: JsonObject?): List<CustomPlugin> =
        (call("minecraft.minigame.customPlugins", buildJsonObject {
            put("loader", loader)
            put("env", env ?: JsonNull)
        }) as JsonArray).map {
            CustomPlugin(
                url = it.jsonObject["url"]!!.jsonPrimitive.content,
                filename = it.jsonObject["filename"]!!.jsonPrimitive.content,
            )
        }

    /**
     * The env vars our own plugins read, and nothing else.
     *
     * A server's settings are **not** the JVM's environment — the supervisor
     * spawns Java with its own — so a plugin calling `System.getenv` sees none
     * of them, and the player's chosen match size never reached the game. This
     * is the curated set that is forwarded, curated rather than passed through
     * because the rest of that map is whatever the dashboard holds for this
     * server.
     */
    fun pluginEnv(env: JsonObject?): Map<String, String> =
        call("minecraft.minigame.pluginEnv", buildJsonObject {
            put("env", env ?: JsonNull)
        }).jsonObject.mapValues { (_, v) -> v.jsonPrimitive.content }

    // -----------------------------------------------------------------------
    // Minecraft accounts
    //
    // The Microsoft sign-in chain. Every request body and every response shape
    // is the core's, because the chain is five calls deep and full of details
    // that fail silently when wrong — the `d=` prefix on the RPS ticket, the
    // relying party that has to be Minecraft's and not Xbox's, the identity
    // token's exact spelling. [MinecraftAuth] performs the calls and decides
    // nothing about them. See `minecraft::account`.
    // -----------------------------------------------------------------------

    /** One HTTP call, as the core described it. */
    data class HttpRequest(
        val method: String,
        val url: String,
        val headers: List<Pair<String, String>>,
        val body: String?,
    )

    private fun httpRequest(value: JsonElement): HttpRequest {
        val obj = value.jsonObject
        return HttpRequest(
            method = obj["method"]!!.jsonPrimitive.content,
            url = obj["url"]!!.jsonPrimitive.content,
            headers = obj["headers"]!!.jsonArray.map {
                val pair = it.jsonArray
                pair[0].jsonPrimitive.content to pair[1].jsonPrimitive.content
            },
            body = obj["body"]?.jsonPrimitive?.contentOrNull,
        )
    }

    /** A pending device-code sign-in, with the page to send the user to. */
    data class DeviceCode(
        val userCode: String,
        val deviceCode: String,
        /** `microsoft.com/link` with the code already filled in. */
        val approvalUrl: String,
        val intervalSecs: Long,
        val expiresInSecs: Long,
    )

    fun accountDeviceCodeRequest(): HttpRequest =
        httpRequest(call("minecraft.account.deviceCodeRequest", buildJsonObject {}))

    fun accountDeviceCodeFrom(body: JsonElement): DeviceCode =
        call("minecraft.account.deviceCodeFrom", buildJsonObject { put("body", body) })
            .jsonObject.let {
                DeviceCode(
                    userCode = it["userCode"]!!.jsonPrimitive.content,
                    deviceCode = it["deviceCode"]!!.jsonPrimitive.content,
                    approvalUrl = it["approvalUrl"]!!.jsonPrimitive.content,
                    intervalSecs = it["intervalSecs"]!!.jsonPrimitive.long,
                    expiresInSecs = it["expiresInSecs"]!!.jsonPrimitive.long,
                )
            }

    fun accountPollRequest(deviceCode: String): HttpRequest =
        httpRequest(call("minecraft.account.pollRequest", buildJsonObject {
            put("deviceCode", deviceCode)
        }))

    /**
     * What one poll meant: `pending`, `slowDown`, `declined`, `expired`, or
     * `approved` with the tokens.
     *
     * Note the first four arrive from Microsoft as HTTP 400. Treating a
     * non-2xx as a failure here would report a sign-in that is working
     * perfectly as broken, once every five seconds.
     */
    fun accountPollOutcome(body: JsonElement): JsonObject =
        call("minecraft.account.pollOutcome", buildJsonObject { put("body", body) }).jsonObject

    /**
     * Microsoft's own token response, normalised.
     *
     * Needed on the refresh path only: a poll outcome has already been through
     * the core and comes back in this crate's spelling, while a refresh returns
     * Microsoft's raw `snake_case` body. Feeding the second straight into
     * [accountSessionFrom] silently produced a session with no tokens in it.
     */
    fun accountMsaTokensFrom(body: JsonElement): JsonObject =
        call("minecraft.account.msaTokensFrom", buildJsonObject { put("body", body) }).jsonObject

    fun accountRefreshRequest(refreshToken: String): HttpRequest =
        httpRequest(call("minecraft.account.refreshRequest", buildJsonObject {
            put("refreshToken", refreshToken)
        }))

    fun accountXblRequest(msaAccessToken: String): HttpRequest =
        httpRequest(call("minecraft.account.xblRequest", buildJsonObject {
            put("msaAccessToken", msaAccessToken)
        }))

    fun accountXstsRequest(xblToken: String): HttpRequest =
        httpRequest(call("minecraft.account.xstsRequest", buildJsonObject {
            put("xblToken", xblToken)
        }))

    fun accountXboxTokenFrom(body: JsonElement): JsonObject =
        call("minecraft.account.xboxTokenFrom", buildJsonObject { put("body", body) }).jsonObject

    /** An XSTS refusal, in words naming what the player has to go and do. */
    fun accountXstsRefusal(body: JsonElement): String =
        call("minecraft.account.xstsRefusal", buildJsonObject { put("body", body) })
            .jsonPrimitive.content

    fun accountMinecraftLoginRequest(xsts: JsonObject): HttpRequest =
        httpRequest(call("minecraft.account.minecraftLoginRequest", buildJsonObject {
            put("xsts", xsts)
        }))

    fun accountMinecraftTokenFrom(body: JsonElement): String =
        call("minecraft.account.minecraftTokenFrom", buildJsonObject { put("body", body) })
            .jsonPrimitive.content

    fun accountProfileRequest(minecraftToken: String): HttpRequest =
        httpRequest(call("minecraft.account.profileRequest", buildJsonObject {
            put("minecraftToken", minecraftToken)
        }))

    /** The stored session: identity plus the tokens that keep it alive. */
    fun accountSessionFrom(
        profile: JsonElement,
        minecraftToken: String,
        msa: JsonElement,
        nowMs: Long,
    ): JsonObject =
        call("minecraft.account.sessionFrom", buildJsonObject {
            put("profile", profile)
            put("minecraftToken", minecraftToken)
            put("msa", msa)
            put("nowMs", nowMs)
        }).jsonObject

    /**
     * The only shape of a session allowed to cross into the WebView.
     *
     * The bridge type has token fields because the desktop's client launcher
     * needs them to start a game. No phone surface reads one, so they go over
     * as `"0"` and the real tokens stay in [SecretStore].
     */
    fun accountRedacted(session: JsonObject): JsonObject =
        call("minecraft.account.redacted", buildJsonObject { put("session", session) }).jsonObject

    fun accountNeedsRefresh(expiresAt: Long, nowMs: Long): Boolean =
        call("minecraft.account.needsRefresh", buildJsonObject {
            put("expiresAt", expiresAt)
            put("nowMs", nowMs)
        }).jsonPrimitive.boolean

    // -----------------------------------------------------------------------
    // Modpacks
    //
    // A `.mrpack` is a zip: a manifest naming mods to fetch by URL, plus an
    // `overrides/` tree copied verbatim. The question it forces is which of
    // those mods must not be installed on a dedicated server — and the answer
    // is the core's, because it is the same question `mods` answers, with two
    // more sources of evidence. See `minecraft::modpack`.
    // -----------------------------------------------------------------------

    /** What to do about a `MODRINTH_MODPACK` value: fetch it, or ask first. */
    fun modpackPlan(modpack: String): JsonObject =
        call("minecraft.modpack.plan", buildJsonObject {
            put("modpack", modpack)
        }).jsonObject

    /** The archive URL, from what the plan's request returned. Null means ask again. */
    fun modpackSourceFrom(of: String, json: JsonElement): JsonObject? =
        call("minecraft.modpack.sourceFrom", buildJsonObject {
            put("of", of)
            put("json", json)
        }).let { it as? JsonObject }

    /** The unfiltered version list, for a pack with no featured release. */
    fun modpackFallbackUrl(modpack: String): String? =
        call("minecraft.modpack.fallbackUrl", buildJsonObject {
            put("modpack", modpack)
        }).jsonPrimitive.contentOrNull

    /** The loader, Minecraft version and pinned loader build a pack needs. */
    fun modpackRequires(manifest: JsonElement): JsonObject =
        call("minecraft.modpack.requires", buildJsonObject {
            put("manifest", manifest)
        }).jsonObject

    /** Start deciding what a pack installs. */
    fun modpackBegin(inputs: JsonObject): JsonObject =
        call("minecraft.modpack.begin", buildJsonObject { put("inputs", inputs) }).jsonObject

    /** Report what the last batch of steps did, and get the next one. */
    fun modpackAdvance(state: JsonElement, replies: JsonArray): JsonObject =
        call("minecraft.modpack.advance", buildJsonObject {
            put("state", state)
            put("replies", replies)
        }).jsonObject

    /**
     * Which assembled jars to prune.
     *
     * Modrinth's dependency data drifts from what a jar's own metadata says,
     * and the loader enforces the jar — so the finished directory is checked
     * against the jars themselves.
     */
    fun modpackReconcile(jars: JsonArray): List<String> =
        call("minecraft.modpack.reconcile", buildJsonObject {
            put("jars", jars)
        }).jsonArray.map { it.jsonPrimitive.content }

    /** Does any `MODRINTH_OVERRIDES_EXCLUSIONS` glob match this override path? */
    fun modpackExcluded(patterns: List<String>, path: String): Boolean =
        call("minecraft.modpack.excluded", buildJsonObject {
            put("patterns", buildJsonArray { patterns.forEach { add(it) } })
            put("path", path)
        }).jsonPrimitive.boolean

    /** What a mod jar declares about itself, from its own metadata entries. */
    fun readModJar(fabric: String?, tomls: List<String>): JsonObject =
        call("minecraft.modjar.read", buildJsonObject {
            fabric?.let { put("fabric", it) }
            put("tomls", buildJsonArray { tomls.forEach { add(it) } })
        }).jsonObject

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
    // The device websocket
    // -----------------------------------------------------------------------

    /**
     * This device's own link, as `link_up` returns it.
     *
     * Not a server's shape: it arrives flat rather than nested under
     * `config.links[]`, which is why it has its own parser rather than a mode
     * flag on [linkFromServerBody].
     */
    data class DeviceLink(
        val link: JsonObject,
        /** The ACME identifier, the TLS SNI, and what the dashboard dials. */
        val fqdn: String?,
        /**
         * The legacy plane is nginx, which prefixes a PROXY v1 header on
         * `:443`; the v2 gateway does not. Whatever terminates TLS has to be
         * told which, because the header arrives where a ClientHello is
         * expected and every handshake fails rather than one warning appearing.
         */
        val expectsProxyProtocol: Boolean,
    )

    /**
     * Read a `link_up` result, or null while the task is still running.
     *
     * Null is not a failure. The API answers with no `native_config` for the
     * first several seconds, and a caller that treats that as one abandons a
     * link that was about to be provisioned.
     */
    fun deviceLinkFromBody(body: JsonElement): DeviceLink? {
        val reply = call("deviceWs.fromLinkUpBody", buildJsonObject { put("body", body) })
        if (reply is JsonNull) return null
        val obj = reply.jsonObject
        return DeviceLink(
            link = obj.getValue("link").jsonObject,
            fqdn = obj["fqdn"]?.jsonPrimitive?.contentOrNull,
            // The core answers `gateway_v2`; this host cares about the
            // consequence rather than the provenance.
            expectsProxyProtocol = obj["gateway_v2"]?.jsonPrimitive?.booleanOrNull != true,
        )
    }

    /**
     * The wireproxy config for the device websocket's own tunnel.
     *
     * A null [httpTarget] omits the ACME forward, which is the shape a device
     * with no certificate takes — forwarding a port at a listener that was
     * never started is worse than not forwarding it.
     */
    fun deviceWsTunnelConfig(link: JsonObject, httpsTarget: Int, httpTarget: Int?): String =
        call("deviceWs.tunnelConfig", buildJsonObject {
            put("link", link)
            put("httpsTarget", httpsTarget)
            httpTarget?.let { put("httpTarget", it) }
        }).jsonPrimitive.content

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
     * Why this device will not host this server, or null to go ahead.
     *
     * Asked *before* the launch plan, because the expensive half of a launch —
     * fetching and unpacking a modpack — happens long before an engine would
     * have a chance to object. A linked engine never objects at all: it starts
     * vanilla and looks like it worked, which is how a player loses their mods
     * without seeing an error.
     *
     * The message is written for a player and is shown as-is. `homerun-core::
     * minecraft::hosting` holds the rules and the reasoning; this passes the
     * server through verbatim, since reducing `game_type` first would hide
     * `native-crossplay`.
     */
    fun hostingRefusal(host: HostEngines, gameType: String, env: JsonObject): String? =
        call("minecraft.hosting.refuse", buildJsonObject {
            put("host", host.json())
            put("server", buildJsonObject {
                put("gameType", gameType)
                put("env", env)
            })
        }).let { if (it is JsonNull) null else it.jsonObject["message"]?.jsonPrimitive?.content }

    /**
     * What this device can actually run, which is the half of the question
     * only the host can answer.
     *
     * Not "which engine is this device" — that had no answer once Android had
     * two. A JRE staged in the APK and a Pumpkin binary in `nativeLibraryDir`
     * are independent facts, and either can be missing from a build.
     */
    data class HostEngines(val jvm: Boolean, val pumpkin: Boolean, val bedrock: Boolean = false) {
        fun json(): JsonObject = buildJsonObject {
            put("jvm", jvm)
            put("pumpkin", pumpkin)
            put("bedrock", bedrock)
        }
    }

    /** Which engine serves a server, or why none of this device's can. */
    data class Serves(val engine: String?, val refusal: String?)

    /**
     * Which of this device's engines runs this server.
     *
     * The routing decision, and it is the core's rather than Kotlin's because
     * it is three rules that iOS needs too: a Pumpkin server goes to Pumpkin
     * and is never substituted, a Java server prefers a real JVM for the mods
     * and plugins only a JVM can run, and a device with no JVM serves a plain
     * Java server with Pumpkin anyway — which is every server that exists on
     * iOS.
     *
     * Returns the refusal in the same call, so a caller needs one round trip
     * rather than asking [hostingRefusal] and then asking again.
     */
    fun serves(host: HostEngines, gameType: String, env: JsonObject): Serves =
        call("minecraft.hosting.serves", buildJsonObject {
            put("host", host.json())
            put("server", buildJsonObject {
                put("gameType", gameType)
                put("env", env)
            })
        }).jsonObject.let {
            Serves(
                engine = it["engine"]?.jsonPrimitive?.contentOrNull,
                refusal = it["refusal"]?.let { r ->
                    if (r is JsonNull) null else r.jsonObject["message"]?.jsonPrimitive?.content
                },
            )
        }

    /**
     * Whether this kind of server has a jar to fetch and a `Main-Class` to
     * read. False for Pumpkin, which *is* the server, and for Bedrock.
     *
     * Feeds [launchPlan]. Without it the plan is inferred from whether the
     * engine is spawned — true of a Pumpkin child process, which would then be
     * sent to download a Mojang jar it cannot use.
     */
    fun needsJvm(gameType: String): Boolean =
        call("minecraft.hosting.needsJvm", buildJsonObject {
            put("gameType", gameType)
        }).jsonPrimitive.boolean

    /** One step of a launch, and whether a pending stop is honoured before it. */
    data class Step(val name: String, val checkpoint: Boolean)

    /**
     * The order a launch runs in.
     *
     * The host performs the steps; it does not choose their order. Several are
     * load-bearing in ways that only show up much later — the jar must land
     * before the restore decides whether a world is on disk, the restore must
     * precede the settings it would otherwise overwrite, and the tunnel must
     * be up before `running` is announced or the API marks a service healthy
     * that no player can reach. `homerun-core::launch` has the reasoning and
     * the tests.
     */
    fun launchPlan(
        backups: Boolean,
        settings: Boolean,
        tunnel: Boolean,
        needsJvm: Boolean? = null,
    ): List<Step> =
        (call("launch.plan", buildJsonObject {
            put("backups", backups)
            put("settings", settings)
            put("tunnel", tunnel)
            // Omitted means "infer it from the engine", which is what this
            // host did before a Pumpkin server could be spawned.
            if (needsJvm != null) put("needsJvm", needsJvm)
        }) as JsonArray).map {
            Step(
                name = it.jsonObject["step"]!!.jsonPrimitive.content,
                checkpoint = it.jsonObject["checkpoint"]?.jsonPrimitive?.boolean == true,
            )
        }

    /**
     * Who owns a server right now, and what its last exit meant.
     *
     * The host reports what only it can see — a call arrived, a process
     * spawned, a process exited — and the core answers what any of it means.
     * Nothing here decides anything; every branch below is the core's.
     *
     * # Why this is not a set of booleans in the backend any more
     *
     * It was, and the same bug was written three times in one week: a server
     * that is *starting* or *stopping* is still this device's, and reporting
     * otherwise makes the UI's reconcile loop take a launch for a remote start
     * and reprovision the gateway underneath it — a tunnel that handshakes and
     * carries nothing. `homerun-core::lifecycle` has that reasoning, and its
     * tests; this is the wire to it.
     *
     * State is opaque and lives here, exactly as [Handshake] does: it goes in,
     * a new one comes back, and there is no native handle to free. All access
     * is synchronised because starts arrive on the bridge's coroutines while
     * exits arrive on the process-watcher thread.
     */
    class Lifecycle(private val concurrency: String) {
        private var state: JsonObject? = null

        /** Everything the core answers about a server after an event. */
        data class View(
            val verdict: String?,
            val serverId: String?,
            val activeIds: List<String>,
            val runningIds: List<String>,
            val state: String,
            val shouldAbandon: Boolean,
            /** A previous engine is still alive; do not spawn until it is gone. */
            val awaitPreviousExit: Boolean,
            /** Starting cancels any on-stop backup of this server still running. */
            val supersedesOnStopBackup: Boolean,
            val intentional: Boolean,
            val superseded: Boolean,
            /** Only answered when a state was asked about; true otherwise. */
            val mayAnnounce: Boolean,
        )

        @Synchronized
        private fun apply(event: String, serverId: String, code: Int? = null): View {
            val reply = call("lifecycle.apply", buildJsonObject {
                state?.let { put("lifecycle", it) }
                put("concurrency", concurrency)
                put("event", event)
                put("serverId", serverId)
                code?.let { put("code", it) }
            }).jsonObject
            state = reply["lifecycle"]!!.jsonObject
            return reply.toView()
        }

        private fun JsonObject.toView() = View(
            verdict = this["verdict"]?.jsonPrimitive?.contentOrNull,
            serverId = this["serverId"]?.jsonPrimitive?.contentOrNull,
            activeIds = ids("activeIds"),
            runningIds = ids("runningIds"),
            state = this["state"]?.jsonPrimitive?.contentOrNull.orEmpty(),
            shouldAbandon = this["shouldAbandon"]?.jsonPrimitive?.boolean == true,
            awaitPreviousExit = this["awaitPreviousExit"]?.jsonPrimitive?.boolean == true,
            supersedesOnStopBackup =
                this["supersedesOnStopBackup"]?.jsonPrimitive?.boolean == true,
            intentional = this["intentional"]?.jsonPrimitive?.boolean == true,
            superseded = this["superseded"]?.jsonPrimitive?.boolean == true,
            mayAnnounce = this["mayAnnounce"]?.jsonPrimitive?.boolean != false,
        )

        private fun JsonObject.ids(key: String): List<String> =
            (this[key] as? JsonArray)?.mapNotNull { it.jsonPrimitive.contentOrNull } ?: emptyList()

        // --- events ---------------------------------------------------------

        /**
         * A start call arrived. Call this *first*, before the lookups a start
         * needs: a server not yet counted active is one the reconcile loop
         * will try to start for itself.
         *
         * Verdict is `proceed`, `alreadyRunning`, or `anotherServerRunning`
         * (with [View.serverId] naming the one in the way).
         */
        fun startRequested(serverId: String): View = apply("startRequested", serverId)

        /** `proceed`, `abandonLaunch` (nothing spawned yet), or `notRunning`. */
        fun stopRequested(serverId: String): View = apply("stopRequested", serverId)

        /** Always, in a `finally` — whatever the verdict was. */
        fun callFinished(serverId: String) {
            apply("callFinished", serverId)
        }

        fun spawned(serverId: String) {
            apply("spawned", serverId)
        }

        fun consoleReady(serverId: String) {
            apply("consoleReady", serverId)
        }

        fun abandoned(serverId: String) {
            apply("abandoned", serverId)
        }

        /** What the exit meant: state, whether it was asked for, whether it
         *  belongs to a launch that has since been replaced. */
        fun exited(serverId: String, code: Int): View = apply("exited", serverId, code)

        // --- queries --------------------------------------------------------

        @Synchronized
        private fun query(serverId: String, announcing: String? = null): View =
            call("lifecycle.query", buildJsonObject {
                state?.let { put("lifecycle", it) }
                put("concurrency", concurrency)
                put("serverId", serverId)
                announcing?.let { put("state", it) }
            }).jsonObject.toView()

        /** `native-server-active-ids`: running, coming up, or winding down. */
        fun activeIds(): List<String> = query("").activeIds

        fun runningIds(): List<String> = query("").runningIds

        /** True when a launch should give up at its next checkpoint. */
        fun shouldAbandon(serverId: String): Boolean = query(serverId).shouldAbandon

        /**
         * True when a previous engine is still alive and this launch must wait
         * before spawning. Asked immediately before spawning, not at
         * admission: the outgoing engine usually exits during the new launch's
         * preparation.
         */
        fun awaitPreviousExit(serverId: String): Boolean = query(serverId).awaitPreviousExit

        /** True when starting must cancel an on-stop backup still running. */
        fun supersedesOnStopBackup(serverId: String): Boolean =
            query(serverId).supersedesOnStopBackup

        /** False when announcing this would contradict a stop already in flight. */
        fun mayAnnounce(serverId: String, state: String): Boolean =
            query(serverId, state).mayAnnounce
    }

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
    // Backups
    // -----------------------------------------------------------------------
    //
    // Decisions only. Nothing here runs an engine or touches a repository —
    // the host does that — which is what lets iOS reach the same answers over
    // the C ABI without a second copy of any of this.

    /** What to do with the local world before launching. */
    sealed class Restore {
        /** A dashboard rollback. Unconditional, and one-shot. */
        data class Rollback(val snapshotId: String) : Restore()
        /** Pull the newest snapshot over the local world. */
        data class Latest(val snapshotId: String, val reason: String) : Restore()
        /** Keep what is on disk. */
        data class Skip(val reason: String) : Restore()
    }

    fun restoreDecision(
        pinned: String?,
        latest: JsonObject?,
        deviceId: String,
        hasLocalWorld: Boolean,
    ): Restore {
        val reply = call("backup.restoreDecision", buildJsonObject {
            pinned?.let { put("pinned", it) }
            latest?.let { put("latest", it) }
            put("deviceId", deviceId)
            put("hasLocalWorld", hasLocalWorld)
        }).jsonObject

        val reason = reply["reason"]?.jsonPrimitive?.contentOrNull.orEmpty()
        return when (reply["action"]?.jsonPrimitive?.contentOrNull) {
            "rollback" -> Restore.Rollback(reply["snapshot_id"]!!.jsonPrimitive.content)
            "restoreLatest" -> Restore.Latest(reply["snapshot_id"]!!.jsonPrimitive.content, reason)
            else -> Restore.Skip(reason)
        }
    }

    /** Whether the backup lease permits this device to launch. */
    sealed class Lease {
        data object Launch : Lease()
        data class Blocked(val device: String) : Lease()
        data class Forced(val takenFrom: String) : Lease()
    }

    fun leaseDecision(leaseDevice: String?, deviceId: String, force: Boolean): Lease {
        val reply = call("backup.leaseDecision", buildJsonObject {
            leaseDevice?.let { put("leaseDevice", it) }
            put("deviceId", deviceId)
            put("force", force)
        }).jsonObject

        return when (reply["action"]?.jsonPrimitive?.contentOrNull) {
            "blocked" -> Lease.Blocked(reply["device"]!!.jsonPrimitive.content)
            "forced" -> Lease.Forced(reply["taken_from"]!!.jsonPrimitive.content)
            else -> Lease.Launch
        }
    }

    /**
     * Whether there is anything worth backing up.
     *
     * A launch that died before generating a world must not push an empty
     * snapshot over a good one — it would become the newest and look fine.
     */
    fun shouldBackUp(hasLocalWorld: Boolean): Boolean =
        call("backup.shouldBackUp", buildJsonObject { put("hasLocalWorld", hasLocalWorld) })
            .jsonPrimitive.boolean

    /** A normalised engine failure. */
    data class Failure(val kind: String, val retryable: Boolean, val succeeded: Boolean)

    fun classifyBackupFailure(exitCode: Int?, message: String, host: String): Failure {
        val reply = call("backup.classify", buildJsonObject {
            exitCode?.let { put("exitCode", it) }
            put("message", message)
            put("host", host)
        }).jsonObject
        return Failure(
            kind = reply["failure"]?.jsonObject?.get("kind")?.jsonPrimitive?.contentOrNull.orEmpty(),
            retryable = reply["retryable"]?.jsonPrimitive?.boolean == true,
            succeeded = reply["succeeded"]?.jsonPrimitive?.boolean == true,
        )
    }

    /** The directory name a snapshot's recorded path ends in. */
    fun recordedBasename(path: String): String? =
        (call("backup.recordedBasename", buildJsonObject { put("path", path) }) as? JsonPrimitive)
            ?.contentOrNull

    /** The `POST /backup-state/` body, and whether sending it frees the lease. */
    data class Report(val body: JsonObject, val releasesLease: Boolean)

    fun backupReport(
        operation: String,
        snapshotId: String? = null,
        error: String? = null,
        bytes: Long = 0,
        durationSeconds: Double = 0.0,
    ): Report {
        val reply = call("backup.stateReport", buildJsonObject {
            put("operation", operation)
            snapshotId?.let { put("snapshotId", it) }
            error?.let { put("error", it) }
            put("bytes", bytes)
            put("durationSeconds", durationSeconds)
        }).jsonObject
        return Report(
            body = reply["body"]!!.jsonObject,
            releasesLease = reply["releasesLease"]?.jsonPrimitive?.boolean == true,
        )
    }

    // -----------------------------------------------------------------------
    // Running the JVM
    // -----------------------------------------------------------------------

    /** The portable half of a Java server's command line. */
    data class Launch(
        val heapMb: Int,
        /** `-Xmx` and `-Xms`, in order. */
        val options: List<String>,
        /** What Minecraft's own main takes. */
        val programArgs: List<String>,
        val eulaFile: String,
        val eulaContents: String,
    )

    /**
     * How much heap, and the flags that carry it.
     *
     * [deviceTotalMb] is the ceiling this device can afford to give away —
     * null on a machine with no such limit. The core decides what fraction of
     * it is safe; this host only measures it.
     */
    fun jvmLaunch(memoryMb: Int, deviceTotalMb: Int?): Launch =
        call("minecraft.jvm.launch", buildJsonObject {
            put("memoryMb", memoryMb)
            deviceTotalMb?.let { put("deviceTotalMb", it) }
        }).jsonObject.let { obj ->
            fun strings(key: String) =
                (obj[key] as? JsonArray).orEmpty().mapNotNull { it.jsonPrimitive.contentOrNull }
            Launch(
                heapMb = obj["heapMb"]?.jsonPrimitive?.intOrNull ?: 1024,
                options = strings("options"),
                programArgs = strings("programArgs"),
                eulaFile = obj["eulaFile"]?.jsonPrimitive?.contentOrNull ?: "eula.txt",
                eulaContents = obj["eulaContents"]?.jsonPrimitive?.contentOrNull ?: "eula=true\n",
            )
        }

    /** One rung of the stop ladder: do this, then wait this long. */
    data class Rung(
        /** `console`, `terminate` or `kill`. */
        val action: String,
        val waitMs: Long,
    )

    /**
     * How to stop a running JVM, in the order to try it.
     *
     * [console] is false when nothing is listening on stdin yet — a server
     * stopped while it was still booting. Why the first rung is not a
     * terminate, and why the waits are what they are, is
     * `homerun_core::minecraft::jvm::stop_ladder`.
     */
    fun stopLadder(console: Boolean): Pair<String, List<Rung>> =
        call("minecraft.jvm.stopLadder", buildJsonObject {
            put("console", console)
        }).jsonObject.let { obj ->
            val command = obj["command"]?.jsonPrimitive?.contentOrNull ?: "stop"
            val rungs = (obj["rungs"] as? JsonArray).orEmpty().map {
                Rung(
                    action = it.jsonObject["action"]?.jsonPrimitive?.contentOrNull.orEmpty(),
                    waitMs = it.jsonObject["waitMs"]?.jsonPrimitive?.longOrNull ?: 0L,
                )
            }
            command to rungs
        }

    /** How long a launch waits for the things it cannot hurry. */
    data class Limits(val startTimeoutMs: Long, val previousExitWaitMs: Long)

    fun jvmLimits(): Limits =
        call("minecraft.jvm.limits", buildJsonObject {}).jsonObject.let {
            Limits(
                startTimeoutMs = it["startTimeoutMs"]?.jsonPrimitive?.longOrNull ?: 300_000L,
                previousExitWaitMs =
                    it["previousExitWaitMs"]?.jsonPrimitive?.longOrNull ?: 120_000L,
            )
        }

    /**
     * The wording for something this host could not do.
     *
     * One sentence per refusal, shared with every other Homerun app. Do not
     * reword at the call site — change it in `jvm::Refusal`.
     */
    fun refusal(kind: String): String =
        call("minecraft.jvm.refusal", buildJsonObject { put("kind", kind) })
            .jsonPrimitive.content

    // -----------------------------------------------------------------------
    // What a run is costing — for a host that must sample itself
    //
    // Two paths reach `homerun-core::metrics`, and which one a backend takes
    // follows from whether the server is a separate process.
    //
    //  - A **child process** is sampled by the supervisor that owns it, in
    //    Rust, and the host only reads the finished graph. That is
    //    `JavaServerBackend`, and it uses none of this.
    //  - A **linked engine** *is* this app. There is no separate process to
    //    measure, so only the host can report anything at all — which is why
    //    `Engine::usage` answers None for one. `PumpkinBackend` takes its own
    //    readings and records them through the class below.
    //
    // The core is the same either way, so the retention rule and the rate
    // arithmetic stay one implementation. Only who holds the tape measure
    // differs.
    // -----------------------------------------------------------------------

    /**
     * One run's performance graph, kept by `homerun-core::metrics`.
     *
     * The host reads **counters** — resident KiB, cumulative CPU seconds — and
     * offers them here. It never computes a percentage: that is a difference
     * between two moments, and it is where wrong graphs come from. The core
     * decides what a reading means, whether it is due, and how much history to
     * keep, so a phone's graph of a server covers the same span as the
     * desktop's graph of the same server.
     *
     * One instance per **run**. A graph covers a session; a restart starts a
     * new one, so [reset] is called from `start`, not from a constructor.
     *
     * State is opaque and lives here, exactly as [Lifecycle]'s does.
     * Synchronised because the sampler runs on its own coroutine while the
     * bridge reads the graph from another.
     */
    class Metrics {
        private var state: JsonObject? = null

        /** Start a fresh session. Everything sampled so far is dropped. */
        @Synchronized
        fun reset() {
            state = null
        }

        /**
         * Offer a reading. Returns true when it became a point on the graph.
         *
         * Offering more often than [intervalMs] is fine and cheap — the extra
         * readings still anchor the next rate, so a one-second pump feeding a
         * thirty-second graph measures over the last second rather than over
         * the whole interval.
         */
        @Synchronized
        fun record(
            atMs: Long,
            memUsedKb: Long?,
            cpuSeconds: Double?,
            playerCount: Int?,
        ): Boolean {
            val reply = call("metrics.record", buildJsonObject {
                state?.let { put("history", it) }
                put("reading", buildJsonObject {
                    put("atMs", atMs)
                    memUsedKb?.let { put("memUsedKb", it) }
                    cpuSeconds?.let { put("cpuSeconds", it) }
                    playerCount?.let { put("playerCount", it) }
                })
            }).jsonObject
            state = reply["history"]!!.jsonObject
            return reply["appended"]?.jsonPrimitive?.boolean == true
        }

        /**
         * How long to wait before offering the next reading.
         *
         * **Re-read this every time.** It doubles when the buffer fills, and a
         * sampler still scheduling on the original keeps paying to read
         * `/proc` at a resolution the core has stopped keeping.
         */
        @Synchronized
        fun intervalMs(): Long =
            query()["intervalMs"]?.jsonPrimitive?.longOrNull ?: 30_000L

        /** The graph, oldest first. */
        @Synchronized
        fun samples(): List<Sample> =
            (query()["samples"] as? JsonArray).orEmpty().map { entry ->
                val obj = entry.jsonObject
                fun number(key: String) = obj[key]?.jsonPrimitive?.contentOrNull?.toDoubleOrNull()
                Sample(
                    t = obj["t"]?.jsonPrimitive?.longOrNull ?: 0L,
                    memUsedMb = number("memUsedMb")?.toInt(),
                    // Not rounded here: an idle server is a fraction of a
                    // percent, and the graph is the only thing entitled to
                    // decide how to show that.
                    cpuPercent = number("cpuPercent"),
                    playerCount = number("playerCount")?.toInt(),
                )
            }

        private fun query(): JsonObject = call("metrics.query", buildJsonObject {
            state?.let { put("history", it) }
        }).jsonObject

        /** One point on a graph. Nulls render as "unavailable", not as zero. */
        data class Sample(
            val t: Long,
            val memUsedMb: Int?,
            val cpuPercent: Double?,
            val playerCount: Int?,
        )
    }

    // -----------------------------------------------------------------------
    // Console
    // -----------------------------------------------------------------------

    /** What one console line means, if anything. */
    data class Line(
        val ready: Boolean,
        val joined: String?,
        val left: String?,
        /** The player ceiling, when the line announced one. */
        val maxPlayers: Int?,
    )

    fun classify(line: String): Line {
        val reply = call("game.classify", buildJsonObject { put("line", line) }).jsonObject
        fun name(key: String) = reply[key]?.let {
            (it as? JsonPrimitive)?.contentOrNull
        }
        return Line(
            ready = reply["ready"]?.jsonPrimitive?.boolean == true,
            joined = name("joined"),
            left = name("left"),
            maxPlayers = reply["maxPlayers"]?.jsonPrimitive?.intOrNull,
        )
    }

    // -----------------------------------------------------------------------
    // Reporting
    // -----------------------------------------------------------------------
    //
    // What the API is told about a run. This app performs the requests and
    // decides none of them — not the path, not the body, and not which
    // credential signs it. See `homerun-core::reporting`.

    /**
     * One API call, decided by the core.
     *
     * [auth] is `device` or `user` and is **not** a detail: the reporting
     * endpoints take the device token, while a settings change is judged
     * against the person who asked for it. Signing with the wrong one is a
     * silent failure in both directions — a 403 nobody sees, or a change
     * attributed to whoever happened to start the server.
     */
    data class Request(
        val method: String,
        val path: String,
        val body: JsonObject,
        val auth: String,
    ) {
        val userSigned: Boolean get() = auth == "user"

        companion object {
            fun from(element: JsonElement?): Request? {
                val obj = (element as? JsonObject) ?: return null
                return Request(
                    method = obj["method"]?.jsonPrimitive?.contentOrNull ?: return null,
                    path = obj["path"]?.jsonPrimitive?.contentOrNull ?: return null,
                    body = obj["body"] as? JsonObject ?: return null,
                    auth = obj["auth"]?.jsonPrimitive?.contentOrNull ?: return null,
                )
            }
        }
    }

    /** What a crash log says went wrong, when the core recognises it. */
    data class Diagnosis(val cause: String, val message: String, val recovery: String) {
        /** The jar was damaged and the budget allows another go at it. */
        val repairable: Boolean get() = recovery == "redownloadAndRestart"
    }

    /**
     * Read a finished run's console.
     *
     * [retriesUsed] is this app's count, not the core's — only the host knows
     * whether a launch ever reached running, which is what resets it. Null
     * means nothing was recognised, and the API's own matching is then the
     * only explanation the player will get.
     */
    fun crashDiagnosis(lines: List<String>, retriesUsed: Int): Diagnosis? {
        val reply = call("reporting.crash.diagnose", buildJsonObject {
            put("lines", buildJsonArray { lines.forEach { add(it) } })
            put("retriesUsed", retriesUsed)
        })
        val obj = reply as? JsonObject ?: return null
        return Diagnosis(
            cause = obj["cause"]?.jsonPrimitive?.contentOrNull ?: return null,
            message = obj["message"]?.jsonPrimitive?.contentOrNull ?: return null,
            recovery = obj["recovery"]?.jsonPrimitive?.contentOrNull ?: return null,
        )
    }

    fun crashReport(serverId: String, deviceId: String, lines: List<String>): Request? =
        Request.from(call("reporting.crash.report", buildJsonObject {
            put("serverId", serverId)
            put("deviceId", deviceId)
            put("lines", buildJsonArray { lines.forEach { add(it) } })
        }))

    fun statsReport(serviceId: String, deviceId: String, stats: JsonObject): Request? =
        Request.from(call("reporting.stats.report", buildJsonObject {
            put("serviceId", serviceId)
            put("deviceId", deviceId)
            put("stats", stats)
        }))

    /** The roster and the world's age, as the supervisor got them. */
    data class Poll(val roster: JsonObject?, val ageSecs: Double?)

    /**
     * Ask the running server for the two things a report needs from it.
     *
     * **Blocking** — it sends a console command and waits for the reply, so
     * call it off the main thread.
     *
     * The supervisor does this rather than this app, because the replies come
     * back as ordinary console lines and only the supervisor can keep them out
     * of the console buffer the UI reads. Filtering them here would be too
     * late: the line is already stored. See `homerun-pumpkin-ffi::server::Ask`.
     *
     * Either field is null on its own — a plugin shadowing `/list` should not
     * cost the gametime.
     */
    fun statsPoll(loader: String): Poll {
        val reply = call("server.statsPoll", buildJsonObject {
            put("loader", loader)
        }) as? JsonObject ?: return Poll(null, null)
        return Poll(
            roster = reply["roster"] as? JsonObject,
            ageSecs = (reply["ageSecs"] as? JsonPrimitive)?.contentOrNull?.toDoubleOrNull(),
        )
    }

    /**
     * Per-core CPU onto the whole device.
     *
     * The sampler measures a process against one core and may exceed 100; this
     * endpoint wants the fraction of the machine. The two agree on a
     * single-core reading, which is why skipping this looks correct until a
     * real phone reports itself on fire.
     */
    fun cpuPercentOfDevice(perCorePercent: Double, cores: Int): Double? =
        (call("reporting.stats.cpuPercentOfDevice", buildJsonObject {
            put("perCorePercent", perCorePercent)
            put("cores", cores)
        }) as? JsonPrimitive)?.contentOrNull?.toDoubleOrNull()

    /**
     * When to report next, and whether now is one of those times.
     *
     * [held] is opaque state kept by the caller between calls, like
     * [Handshake] — no schedule means a run that has just begun, and the first
     * poll is due immediately. [event] is `presence` for a join or leave, or
     * null to simply ask.
     */
    data class Schedule(
        val held: JsonObject,
        /** `periodic`, `presence`, or null when nothing is due yet. */
        val trigger: String?,
        val waitMs: Long,
    )

    fun schedule(held: JsonObject?, nowMs: Long, event: String? = null): Schedule {
        val reply = call("reporting.stats.schedule", buildJsonObject {
            held?.let { put("schedule", it) }
            put("nowMs", nowMs)
            event?.let { put("event", it) }
        }).jsonObject
        return Schedule(
            held = reply["schedule"]!!.jsonObject,
            trigger = (reply["trigger"] as? JsonPrimitive)?.contentOrNull,
            waitMs = reply["waitMs"]?.jsonPrimitive?.longOrNull ?: 0L,
        )
    }

    /**
     * Whether this server verifies accounts with Mojang.
     *
     * Derived once, by the core, from the same inputs that write
     * `server.properties` — the desktop computes it twice from two places and
     * the two disagree, which is what silently breaks op-ing.
     */
    fun onlineMode(settings: HomerunApi.ServerSettings): Boolean? =
        (call("minecraft.settings.fromEnv", buildJsonObject {
            put("env", settings.env)
            put("gameType", settings.rawGameType)
            put("loader", settings.loader)
        }) as? JsonObject)?.get("onlineMode")?.jsonPrimitive?.booleanOrNull

    /**
     * Where a player connects, from a `GET /api/server/<id>/` body.
     *
     * Null until the gateway has assigned an external port, which is the
     * normal state during the first moments of a launch.
     */
    fun publicAddress(body: JsonElement): String? =
        (call("link.publicAddress", buildJsonObject { put("body", body) })
            as? JsonPrimitive)?.contentOrNull

    /**
     * Round-trip time to the gateway address, in milliseconds.
     *
     * Null for every ordinary failure — unreachable, not a Minecraft server,
     * timed out. One optional field on a report is not worth failing over.
     * The socket is the native side's, not this app's: the codec around it is
     * the difficult half and iOS should not have to write a second one.
     */
    fun gatewayPing(address: String): Double? {
        val host = address.substringBeforeLast(':', "").ifEmpty { return null }
        val port = address.substringAfterLast(':', "").toIntOrNull() ?: return null
        return (call("net.gatewayPing", buildJsonObject {
            put("host", host)
            put("port", port)
        }) as? JsonPrimitive)?.contentOrNull?.toDoubleOrNull()
    }

    /**
     * Round-trip time to a region's gateway, in milliseconds, or null when it
     * could not be reached.
     *
     * The argument is the API's `domain` for a region — a bare hostname, not a
     * URL. Splitting it and opening the socket both belong to the native side:
     * doing either here is what produced the bug where every region reported
     * unreachable without a packet being sent. See `docs/region-latency.md`.
     *
     * > **Blocking**, up to a five-second deadline. Call it off the main thread.
     */
    fun regionLatency(domain: String): Double? =
        (call("net.regionLatency", buildJsonObject { put("domain", domain) })
            as? JsonPrimitive)?.contentOrNull?.toDoubleOrNull()

    /**
     * An `op`, `deop`, `ban` or `pardon` typed into the console, if that is
     * what this was. Null for everything else, which is almost every command.
     */
    fun opsCommand(command: String): JsonObject? =
        call("minecraft.ops.parse", buildJsonObject { put("command", command) }) as? JsonObject

    /** The saved settings change, and the line to echo once it has landed. */
    data class OpsChange(val request: Request, val line: String)

    /**
     * What that command should change on the API, given what it currently
     * holds. Null means the list already says this — a `/op` for somebody who
     * is already an operator is not a change to save.
     */
    fun opsSync(command: JsonObject, serverBody: JsonElement, serverId: String): OpsChange? {
        val reply = call("minecraft.ops.sync", buildJsonObject {
            put("command", command)
            put("server", serverBody)
            put("serverId", serverId)
        }) as? JsonObject ?: return null
        return OpsChange(
            request = Request.from(reply["request"]) ?: return null,
            line = reply["line"]?.jsonPrimitive?.contentOrNull ?: return null,
        )
    }

    /** A match a server plugin announced, if this line announced one. */
    fun minigameReport(serverId: String, line: String): Request? =
        Request.from(call("reporting.minigame.fromLine", buildJsonObject {
            put("serverId", serverId)
            put("line", line)
        }))

    // -----------------------------------------------------------------------
    // Over-the-air UI bundles
    // -----------------------------------------------------------------------

    /** A manifest whose signature verified, and what to do about it. */
    data class Offer(
        /** True only when this bundle should be fetched. */
        val install: Boolean,
        /** One sentence for the log, worded once in Rust so both hosts say it the same way. */
        val reason: String,
        val bundle: String,
        val url: String,
        val sha256: String,
        val minHost: Int,
        val serial: Long,
    )

    /**
     * Verify a manifest's signature and judge it against what is installed.
     *
     * One call rather than two, and that is load-bearing: there is no way to
     * get the fields of a manifest whose signature has not been checked. A host
     * that could do that would keep working perfectly against any manifest
     * anyone served it — a bug with no symptom until it is an incident.
     *
     * @throws CoreException if the signature does not verify or the manifest is
     *   malformed. Both mean the same thing to the caller: fetch nothing.
     */
    fun evaluateBundle(manifestJson: String, publicKey: String, installed: JsonObject): Offer {
        val reply = call("bundle.evaluate", buildJsonObject {
            put("manifest", manifestJson)
            put("publicKey", publicKey)
            put("installed", installed)
        }).jsonObject
        val manifest = reply["manifest"]?.jsonObject
            ?: throw CoreException("The core verified a manifest but returned none.")
        fun required(name: String): String = manifest[name]?.jsonPrimitive?.contentOrNull
            ?: throw CoreException("The verified manifest has no $name.")
        return Offer(
            install = reply["install"]?.jsonPrimitive?.boolean ?: false,
            reason = reply["reason"]?.jsonPrimitive?.contentOrNull ?: "no reason given",
            bundle = required("bundle"),
            url = required("url"),
            sha256 = required("sha256"),
            minHost = manifest["minHost"]?.jsonPrimitive?.intOrNull ?: 0,
            serial = manifest["serial"]?.jsonPrimitive?.longOrNull ?: 0L,
        )
    }

    /** Whether a digest this host computed is the one that was signed. */
    fun digestMatches(expected: String, actual: String): Boolean =
        call("bundle.digestMatches", buildJsonObject {
            put("expected", expected)
            put("actual", actual)
        }).jsonPrimitive.boolean
}
