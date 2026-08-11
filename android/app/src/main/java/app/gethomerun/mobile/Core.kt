package app.gethomerun.mobile

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
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
    fun launchPlan(backups: Boolean, settings: Boolean, tunnel: Boolean): List<Step> =
        (call("launch.plan", buildJsonObject {
            put("backups", backups)
            put("settings", settings)
            put("tunnel", tunnel)
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
    // What a run is costing
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
