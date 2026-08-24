package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.add
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * The bits of the Homerun backend this host has to read for itself.
 *
 * The UI passes only a name and a memory ceiling down `native-server-start`;
 * everything else about a server — which Minecraft version, which loader —
 * lives on the backend and is fetched at launch. The desktop does the same in
 * `nativeServerManager.fetchServerConfig`, and for the same reason: settings
 * changed on the web dashboard have to take effect on the next start without
 * the app having been told.
 *
 * Deliberately tiny. The UI owns all other backend traffic; this exists only
 * because the host has to launch a server with no page in front of it.
 */
object HomerunApi {

    /** What launching a server needs to know that the UI does not send. */
    data class ServerSettings(
        /** `null` means the API named none — the latest release, as on desktop. */
        val version: String?,
        /** `vanilla`, `paper`, `fabric`, … — the API's `TYPE`, normalised. */
        val loader: String,
        /** `java` or `bedrock`. Android hosts only the former. */
        val gameType: String,
        /**
         * The API's game type verbatim, before [gameType] collapses it.
         *
         * [gameType] cannot answer whether this is `native-crossplay`, and
         * that distinction decides online mode — a crossplay vanilla server
         * runs offline, because Geyser clients have no Mojang account to
         * verify. `homerun-core::settings` needs the unreduced value.
         */
        val rawGameType: String,
        /**
         * `environment_variables`, untouched.
         *
         * Every world setting a player chose in the creation wizard arrives
         * in here. Reading it is `homerun-core::settings`' job rather than
         * this layer's, so that the desktop and this app cannot disagree
         * about what any of these keys mean.
         */
        val env: JsonObject,
        /**
         * The repository credentials and retention policy, or null when the
         * server has no volume or the feature is off for it.
         *
         * Handed down whole by `get_backup` — the URL is assembled API-side by
         * `build_restic_repo_url`, and the retention policy is the API's
         * choice, not ours. A client that assembled either would be a second
         * place for the rest-server topology to live.
         */
        val backup: JsonObject?,
        /**
         * The device holding the backup lease, or null.
         *
         * Single-writer coordination for the friend-hosting handoff: the API
         * opens the lease on a `stopped` ack that says `backup_in_progress`,
         * and closes it when that device reports `backup-state`. Launching
         * while another device holds it would start a second world from a
         * snapshot still being written.
         */
        val backupLeaseDevice: String?,
        /**
         * A snapshot the dashboard staged for restore (`RESTORE_FROM_SNAPSHOT`).
         *
         * One-shot: the API clears the pin on the next running ack, so acting
         * on it once is the whole contract.
         */
        val restoreFromSnapshot: String?,
        /**
         * The tunnel credentials as they stood *before* launch, if any.
         *
         * Carried on this response rather than fetched separately because
         * that is where the desktop gets it (`_currentNativeConfig` in
         * `fetchServerConfig`), and one round trip is enough. It is the
         * baseline the post-launch poll compares against — the legacy
         * provisioner mints new keys per session, so a config still equal to
         * this one is the dead previous set.
         */
        val tunnelBefore: WireProxy.Link?,
    )

    /** Mirrors the desktop's whitelist; anything else is treated as vanilla. */
    private val LOADERS = setOf(
        "fabric", "forge", "neoforge", "paper", "quilt", "spigot", "bukkit",
    )

    private const val CONNECT_TIMEOUT_MS = 15_000
    private const val READ_TIMEOUT_MS = 20_000

    /** Where the desktop asks too, so both platforms report the same field. */
    private const val PUBLIC_IP_URL = "https://api.ipify.org/?format=json"

    /**
     * What this device registers as — `DeviceType.MOBILE_ANDROID` on the API.
     *
     * The slash is not a path and not a typo: device and game types are both
     * slash-namespaced on the backend (`minecraft/native`), and the API
     * matches this string exactly against its enum. iOS sends `mobile/ios`.
     */
    private const val DEVICE_TYPE = "mobile/android"

    private val json = Json { ignoreUnknownKeys = true }

    /**
     * This element as an object, or null — including when it is JSON `null`.
     *
     * `element?.jsonObject` looks like it does this and does not. A missing key
     * is a Kotlin `null` and `?.` handles it; a key present with the value
     * `null` is `JsonNull`, which is a perfectly non-null Kotlin object, so
     * `?.` sails past it and `jsonObject` throws
     * `Element class JsonNull is not a JsonObject`.
     *
     * That difference cost a whole feature. `backup` is `null` on any server
     * with no repository — which is *every* minigame lobby, since they are
     * ephemeral and deliberately never backed up — so reading its settings
     * threw, [serverSettings] returned null, and the caller took its
     * "settings could not be read" path: vanilla, latest version, no plugins.
     * The server started, the UI said the game was live, and the game was not
     * in it. A launch that silently ignores everything the server was
     * configured with is the worst possible way for this to fail, and it is
     * what an exception in this function buys.
     */
    private fun JsonElement?.objectOrNull(): JsonObject? = this as? JsonObject

    /**
     * Read a server's settings, or null if they could not be read.
     *
     * Null is a normal outcome, not an error: no token yet, no signal, a
     * backend hiccup. The caller falls back to vanilla-latest exactly as the
     * desktop does — refusing to start a server because a settings lookup
     * failed would be worse than starting the default one.
     *
     * It is a *bad* outcome all the same, and one worth being loud about: the
     * fallback is a different server from the one the player configured. Every
     * field below is therefore read defensively rather than optimistically —
     * one absent key must not cost the other fifteen.
     */
    suspend fun serverSettings(
        apiUrl: String,
        serverId: String,
        token: String,
    ): ServerSettings? = withContext(Dispatchers.IO) {
        if (token.isBlank()) {
            Log.i(TAG, "no token for $serverId — using defaults")
            return@withContext null
        }

        try {
            val body = get(apiUrl, "/api/server/$serverId/", token)
                ?: return@withContext null

            val config = body["config"].objectOrNull()
            val env = config?.get("environment_variables").objectOrNull()
            val type = env?.get("TYPE")?.jsonPrimitive?.contentOrNull?.lowercase()
            val gameType = (
                body["game_type"]?.jsonPrimitive?.contentOrNull
                    ?: config?.get("game_type")?.jsonPrimitive?.contentOrNull
                ).orEmpty()

            ServerSettings(
                version = env?.get("VERSION")?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() },
                loader = if (type in LOADERS) type!! else "vanilla",
                gameType = if (gameType == "bedrock" || gameType == "native-bedrock") "bedrock" else "java",
                rawGameType = gameType,
                env = env ?: JsonObject(emptyMap()),
                // Null for every server with no repository, and that is not an
                // edge case — a minigame lobby never has one. See
                // [objectOrNull]; this line read `?.jsonObject` and threw.
                backup = body["backup"].objectOrNull(),
                backupLeaseDevice = body["backup_lease_device"]?.jsonPrimitive?.contentOrNull
                    ?.takeIf { it.isNotBlank() },
                restoreFromSnapshot = env?.get("RESTORE_FROM_SNAPSHOT")?.jsonPrimitive
                    ?.contentOrNull?.takeIf { it.isNotBlank() },
                tunnelBefore = linkOf(body)?.link,
            ).also { Log.i(TAG, "$serverId: ${it.loader} ${it.version ?: "latest"} (${it.gameType})") }
        } catch (err: Exception) {
            Log.w(TAG, "could not read settings for $serverId: ${err.message}")
            null
        }
    }

    /**
     * Wait for the gateway to hand this server a tunnel, then return it.
     *
     * The API provisions the WireGuard peer asynchronously once the server is
     * marked running, so at launch the credentials usually do not exist yet.
     * The desktop polls for them and so does this — same 3 s interval, same 20
     * attempts. Runs in parallel with the server booting, because a minute
     * spent waiting here is a minute the world could have been generating.
     *
     * Null means no tunnel: no token, no signal, or the gateway never
     * provisioned. The caller decides what to do about it — on mobile that is
     * serious, since there is no port-forwarding fallback.
     *
     * @param stale the config seen before launch. The legacy provisioner
     *   regenerates keys per session, so a config identical to what was there
     *   before is the *old* one and using it fails the handshake. Gateway v2
     *   reuses credentials deliberately, so for those this check is skipped —
     *   without that exception a v2 link would poll until timeout every time.
     */
    suspend fun awaitTunnel(
        apiUrl: String,
        serverId: String,
        token: String,
        stale: WireProxy.Link? = null,
        attempts: Int = 20,
        intervalMs: Long = 3_000,
    ): WireProxy.Link? = withContext(Dispatchers.IO) {
        if (token.isBlank()) {
            Log.i(TAG, "no token — $serverId will have no tunnel")
            return@withContext null
        }

        repeat(attempts) { attempt ->
            delay(intervalMs)
            val link = runCatching { readLink(apiUrl, serverId, token) }
                .onFailure { Log.w(TAG, "tunnel poll ${attempt + 1} failed: ${it.message}") }
                .getOrNull()
                ?: return@repeat

            if (!link.isGateway2 && stale != null && link.link == stale) {
                Log.i(TAG, "tunnel poll ${attempt + 1}: still the pre-launch config, waiting")
                return@repeat
            }
            Log.i(TAG, "tunnel ready for $serverId after ${attempt + 1} attempts")
            return@withContext link.link
        }
        Log.w(TAG, "no tunnel for $serverId after $attempts attempts")
        null
    }

    /**
     * Bring this **device's** own link up, and wait for the gateway to
     * provision it.
     *
     * Two calls rather than one: the POST starts a task and returns its id,
     * and the result is polled separately. That is the API's shape, and the
     * desktop polls it the same way — see `launchDeviceWebsocket`.
     *
     * A poll that finds no config yet is the normal state for the first
     * several seconds, not a failure. Null means the gateway never answered
     * inside the window, which leaves this device unreachable by name and
     * breaks nothing else.
     */
    suspend fun awaitDeviceLink(
        apiUrl: String,
        deviceId: String,
        token: String,
        attempts: Int = 20,
        intervalMs: Long = 3_000,
    ): Core.DeviceLink? = withContext(Dispatchers.IO) {
        if (token.isBlank()) {
            Log.i(TAG, "no token — this device will serve no websocket")
            return@withContext null
        }

        val path = "/api/device/$deviceId/link_up/"
        val task = runCatching {
            post(apiUrl, path, buildJsonObject { }, token)
                ?.get("task")?.jsonPrimitive?.contentOrNull
        }.onFailure { Log.w(TAG, "link_up failed: ${it.message}") }.getOrNull()
            ?: return@withContext null
        Log.i(TAG, "link_up triggered, task $task")

        repeat(attempts) { attempt ->
            delay(intervalMs)
            // The API answers 404 while the task is still running, so a failed
            // GET here is "not yet" and not an error worth reporting.
            val body = runCatching { get(apiUrl, "$path?result=$task", token) }
                .onFailure { Log.d(TAG, "link_up poll ${attempt + 1}: ${it.message}") }
                .getOrNull()
                ?: return@repeat

            val link = runCatching { Core.deviceLinkFromBody(body) }
                .onFailure { Log.w(TAG, "link_up result unreadable: ${it.message}") }
                .getOrNull()
                ?: return@repeat

            Log.i(TAG, "device link ready after ${attempt + 1} attempts (fqdn=${link.fqdn})")
            return@withContext link
        }
        Log.w(TAG, "no device link after $attempts attempts")
        null
    }

    /** A link plus the one field that changes how staleness is judged. */
    private data class PolledLink(val link: WireProxy.Link, val isGateway2: Boolean)

    /** The current `native_config`, or null when the gateway has not written one. */
    private fun readLink(apiUrl: String, serverId: String, token: String): PolledLink? {
        val body = get(apiUrl, "/api/server/$serverId/", token) ?: return null
        // The player-facing address rides along on a poll that was happening
        // anyway. It cannot be read at launch: the gateway assigns the
        // external port while this poll is waiting for it, so asking earlier
        // reliably answers null. The desktop caches it here too, in
        // `cacheGatewayHost`, for the same reason.
        runCatching { Core.publicAddress(body) }.getOrNull()
            ?.let { Reporting.gatewayAddressResolved(serverId, it) }
        return linkOf(body)
    }

    /** Pull the tunnel out of a `/api/server/<id>/` body. */
    private fun linkOf(body: JsonObject): PolledLink? {
        // `as?` throughout for the reason [objectOrNull] gives: a key whose
        // value is JSON `null` is not a missing key, and `?.jsonObject` on one
        // throws rather than yielding null. `native_config` already knew that;
        // the two above it did not.
        val link = (body["config"].objectOrNull()?.get("links") as? JsonArray)
            ?.firstOrNull().objectOrNull() ?: return null
        val native = link["native_config"].objectOrNull() ?: return null

        fun str(key: String) = native[key]?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() }

        // Without all three there is no tunnel to build, and a half-written
        // config would fail as an unexplained handshake timeout instead.
        val privateKey = str("client_privkey") ?: return null
        val publicKey = str("gateway_pubkey") ?: return null
        val endpoint = str("link_address") ?: return null

        return PolledLink(
            WireProxy.Link(
                clientPrivateKey = privateKey,
                gatewayPublicKey = publicKey,
                endpoint = endpoint,
                address = str("address"),
                allowedIps = str("allowed_ips"),
            ),
            isGateway2 = link["provisioner"]?.jsonPrimitive?.contentOrNull == "gateway2",
        )
    }

    // -----------------------------------------------------------------------
    // Device registration and reporting
    // -----------------------------------------------------------------------

    /**
     * Register this device, or re-attach to an existing registration.
     *
     * One call does everything: creates the device, adds it to the user's
     * default group and that group's gateway service, joins it to servers the
     * user already has, and issues a device token. Authorised with the
     * **user** token — owner and matrix id are derived server-side, so the
     * client sends only a name and what kind of device it is.
     *
     * [DEVICE_TYPE] is the one thing the backend cannot work out for itself.
     * Without it a phone registers as `native_java` and is indistinguishable
     * from a desktop running the native path — which is what shipped first,
     * and why anything counting phones could not. The API constrains this to
     * the types a client is allowed to claim; `wsl` and `both` are refused.
     *
     * [existingDeviceId] must be an id the backend issued. It is looked up as
     * a primary key owned by this user, so anything invented locally 404s.
     */
    suspend fun registerDevice(
        apiUrl: String,
        userToken: String,
        deviceName: String,
        existingDeviceId: String?,
    ): DeviceRegistry.Registration? = withContext(Dispatchers.IO) {
        val body = buildJsonObject {
            put("device_name", deviceName)
            put("device_type", DEVICE_TYPE)
            if (existingDeviceId != null) put("existing_device_id", existingDeviceId)
        }

        val response = runCatching {
            post(apiUrl, "/api/init/native/", body, userToken)
        }.onFailure {
            Log.w(TAG, "device registration failed: ${it.message}")
        }.getOrNull() ?: return@withContext null

        val deviceId = response["device_id"]?.jsonPrimitive?.contentOrNull
        val deviceToken = response["device_token"]?.jsonPrimitive?.contentOrNull
        if (deviceId == null || deviceToken == null) {
            Log.w(TAG, "registration response had no device_id/device_token")
            return@withContext null
        }
        DeviceRegistry.Registration(
            deviceId = deviceId,
            deviceToken = deviceToken,
            groupId = response["group_id"]?.jsonPrimitive?.contentOrNull,
        )
    }

    /**
     * The heartbeat. Authorised with the **device** token.
     *
     * An empty [instances] list is still a valid report — it is what keeps the
     * device itself marked online, separately from any server.
     */
    suspend fun reportInstances(
        apiUrl: String,
        deviceId: String,
        deviceToken: String,
        instances: List<String>,
    ) = withContext(Dispatchers.IO) {
        val body = buildJsonObject {
            put("instances", buildJsonArray { instances.forEach { add(JsonPrimitive(it)) } })
            put("unacked_tasks", 0)
        }
        runCatching { post(apiUrl, "/api/reporting/device/$deviceId/instances/", body, deviceToken) }
            .onFailure { Log.d(TAG, "heartbeat failed: ${it.message}") }
        Unit
    }

    /**
     * Record that the player asked for this server to be **off**.
     *
     * Not a status report — this is the same PATCH the dashboard sends when
     * someone presses Stop, signed with the **user** token, because it changes
     * what the server is meant to be doing rather than describing what it is
     * doing. [reportServerState] below is the other half and uses the device
     * token; the two are not interchangeable.
     *
     * It exists because the notification's Stop action had no way to say this.
     * Stopping from the app worked only because the page PATCHed afterwards
     * (see the comment on `ServerHost.stop`), and the notification is the one
     * control used when there is no page — so `target_state` stayed "running",
     * `useNativeServerReconcile` saw a server that should be up and was not,
     * and started it again. What the player saw was Stop working and then
     * "Starting…" a few seconds later.
     *
     * Best-effort and quiet on failure, like every other call here: the server
     * really has stopped by the time this runs, and a failed PATCH means the
     * reconcile may restart it — bad, but not worth taking the service down
     * over, and the next in-app Stop corrects it.
     */
    suspend fun markStopped(
        apiUrl: String,
        serverId: String,
        serverName: String?,
        userToken: String,
    ): Boolean = withContext(Dispatchers.IO) {
        if (userToken.isBlank()) {
            Log.w(TAG, "no user token — cannot record the stop of $serverId")
            return@withContext false
        }
        val body = buildJsonObject {
            put("status", JsonPrimitive("stopped"))
            // The dashboard sends the name alongside the status. A blank one is
            // omitted rather than sent, which would rename the server to "".
            serverName?.takeIf { it.isNotBlank() }?.let {
                put("server_name", JsonPrimitive(it))
            }
        }
        runCatching { patch(apiUrl, "/api/server/$serverId/", body, userToken) }
            .onFailure { Log.w(TAG, "could not record the stop of $serverId: ${it.message}") }
            .isSuccess
    }

    /**
     * Acknowledge a server's state, with the **device** token. This is the
     * report the API and the web dashboard wait on — the bridge event of the
     * same name only reaches the page in front of us.
     */
    suspend fun reportServerState(
        apiUrl: String,
        serverId: String,
        state: String,
        deviceToken: String,
        backupInProgress: Boolean = false,
    ) = withContext(Dispatchers.IO) {
        val body = buildJsonObject {
            put("status", state)
            // This flag is what *opens* the backup lease. Sending it commits
            // this device to reporting `backup-state` afterwards — the lease
            // has no timeout, so a device that claims it and never reports
            // locks every other device out until its own next running ack.
            if (backupInProgress) put("backup_in_progress", true)
        }
        runCatching { post(apiUrl, "/api/server/$serverId/state/", body, deviceToken) }
            .onFailure { Log.w(TAG, "state report ($state) failed for $serverId: ${it.message}") }
        Unit
    }

    /**
     * Report the outcome of a backup or restore.
     *
     * For a backup this is also what **releases the lease**, on success and on
     * failure alike — a failed backup that held it forever would strand the
     * server. The body comes from `homerun-core`, so its field names cannot
     * drift from what the endpoint reads.
     *
     * Best-effort: a reporting failure must never turn a completed backup into
     * a failed one. It does mean the lease stays open, which is why the log
     * line says so rather than being silent.
     */
    suspend fun reportBackupState(
        apiUrl: String,
        serverId: String,
        body: JsonObject,
        deviceToken: String,
    ) = withContext(Dispatchers.IO) {
        runCatching { post(apiUrl, "/api/server/$serverId/backup-state/", body, deviceToken) }
            .onFailure {
                Log.w(TAG, "backup-state report failed for $serverId (lease may stay open): ${it.message}")
            }
        Unit
    }

    /** One authenticated POST, parsed. Throws on a non-2xx. */
    /**
     * A server record, whole.
     *
     * For the read half of a read-modify-write: the ops sync has to see the
     * environment variables as the API currently holds them, not as they were
     * at launch, because another device or the dashboard may have changed them
     * since.
     */
    suspend fun serverBody(apiUrl: String, serverId: String, token: String): JsonObject? =
        withContext(Dispatchers.IO) {
            if (token.isBlank()) return@withContext null
            runCatching { get(apiUrl, "/api/server/$serverId/", token) }
                .onFailure { Log.w(TAG, "could not read $serverId: ${it.message}") }
                .getOrNull()
        }

    /**
     * Carry out a request the core decided on.
     *
     * The one entry point for everything in `homerun-core::reporting`: the
     * path, the body and the method all arrive already chosen, and this
     * supplies only the credential the core asked to be signed with and the
     * connection to send it over.
     *
     * Never throws. A report that does not arrive is a gap in a graph; a
     * report that interrupts hosting is a session lost, and every caller here
     * is on a path where the server is more important than the telemetry.
     */
    suspend fun perform(
        apiUrl: String,
        request: Core.Request,
        token: String,
    ): JsonObject? = withContext(Dispatchers.IO) {
        if (token.isBlank()) return@withContext null
        runCatching {
            when (request.method) {
                "patch" -> patch(apiUrl, request.path, request.body, token)
                else -> post(apiUrl, request.path, request.body, token)
            }
        }.onFailure {
            Log.w(TAG, "${request.method} ${request.path} did not go through: ${it.message}")
        }.getOrNull()
    }

    /**
     * Carry out an app error report, signed if this device has a credential
     * and unsigned if it does not.
     *
     * The one request in this file that may go out with no `Authorization`
     * header, and the endpoint is built to accept that. The reason is the
     * whole reason the endpoint exists: the errors worth most are the ones
     * that happen before there is a token to sign with — a crash on the login
     * screen, a failure during device registration, a bundle that throws
     * before the page boots. Requiring a credential would lose exactly those
     * and keep the ones that were already survivable.
     *
     * An unsigned report is attributed to nothing and is rate-limited far
     * harder at the far end; a signed one carries the device and its owner.
     * Both beat silence.
     *
     * Never throws, like [perform].
     */
    suspend fun performAppError(
        apiUrl: String,
        request: Core.Request,
        token: String?,
    ): JsonObject? = withContext(Dispatchers.IO) {
        runCatching { post(apiUrl, request.path, request.body, token) }
            .onFailure {
                // A plain log, never a report. Reporting a failed report is
                // how a reporter becomes the outage.
                Log.w(TAG, "an error report did not go through: ${it.message}")
            }
            .getOrNull()
    }

    /**
     * This device's public address, as the API records it.
     *
     * Cached for the life of the process. It is one fact about the network the
     * phone is on, and asking a third party for it on every report would be
     * both slower and more traffic to somewhere the user did not choose.
     *
     * Deliberately unauthenticated and deliberately not our own API — the same
     * service the desktop uses (`fetchPublicIpAddress`), so both platforms
     * report the same field the same way.
     */
    @Volatile
    private var cachedPublicIp: String? = null

    suspend fun publicIpAddress(): String? = withContext(Dispatchers.IO) {
        cachedPublicIp?.let { return@withContext it }
        val resolved = runCatching {
            val connection = (URL(PUBLIC_IP_URL).openConnection() as HttpURLConnection).apply {
                connectTimeout = CONNECT_TIMEOUT_MS
                readTimeout = READ_TIMEOUT_MS
                setRequestProperty("Accept", "application/json")
            }
            try {
                if (connection.responseCode != HttpURLConnection.HTTP_OK) return@runCatching null
                val text = connection.inputStream.bufferedReader().use { it.readText() }
                (json.parseToJsonElement(text) as? JsonObject)
                    ?.get("ip")?.jsonPrimitive?.contentOrNull
            } finally {
                connection.disconnect()
            }
        }.getOrNull()
        cachedPublicIp = resolved
        resolved
    }

    /**
     * PATCH, which works here and would not on a desktop JVM.
     *
     * The JDK's `HttpURLConnection` rejects `PATCH` outright — its method list
     * is a hardcoded array and `setRequestMethod` throws `ProtocolException`.
     * Android's is OkHttp behind the same interface and allows it. So this is
     * platform-specific in a way that reads as ordinary code — and if it ever
     * stopped being true the failure would be a thrown `ProtocolException` on
     * the ops-sync path, caught by [perform] and logged, not a silent no-op.
     */
    private fun patch(
        apiUrl: String,
        path: String,
        body: JsonObject,
        token: String?,
    ): JsonObject? = send("PATCH", apiUrl, path, body, token)

    private fun post(
        apiUrl: String,
        path: String,
        body: JsonObject,
        token: String?,
    ): JsonObject? = send("POST", apiUrl, path, body, token)

    private fun send(
        method: String,
        apiUrl: String,
        path: String,
        body: JsonObject,
        token: String?,
    ): JsonObject? {
        val connection = (URL("${apiUrl.trimEnd('/')}$path").openConnection() as HttpURLConnection)
            .apply {
                requestMethod = method
                doOutput = true
                connectTimeout = CONNECT_TIMEOUT_MS
                readTimeout = READ_TIMEOUT_MS
                // Only when there is one. An app error report may be the
                // one request this app ever makes unsigned — see
                // [performAppError].
                if (!token.isNullOrBlank()) {
                    setRequestProperty("Authorization", "Bearer $token")
                }
                setRequestProperty("Content-Type", "application/json")
                setRequestProperty("Accept", "application/json")
            }
        try {
            connection.outputStream.use { it.write(body.toString().toByteArray()) }
            val code = connection.responseCode
            if (code !in 200..299) {
                // The error body is where the API says *why* — "Device with id
                // … does not exist" and friends. Losing it turns a precise
                // failure into "HTTP 400".
                val detail = runCatching {
                    connection.errorStream?.bufferedReader()?.use { it.readText() }
                }.getOrNull().orEmpty().take(400)
                throw IOException("HTTP $code from $path${if (detail.isBlank()) "" else ": $detail"}")
            }
            val text = connection.inputStream.bufferedReader().use { it.readText() }
            return if (text.isBlank()) null
            else json.parseToJsonElement(text) as? JsonObject
        } finally {
            connection.disconnect()
        }
    }

    /** One authenticated GET, parsed. Null on any non-200 or transport error. */
    private fun get(apiUrl: String, path: String, token: String): JsonObject? {
        val connection = (URL("${apiUrl.trimEnd('/')}$path").openConnection() as HttpURLConnection)
            .apply {
                connectTimeout = CONNECT_TIMEOUT_MS
                readTimeout = READ_TIMEOUT_MS
                setRequestProperty("Authorization", "Bearer $token")
                setRequestProperty("Accept", "application/json")
            }
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            return json.parseToJsonElement(
                connection.inputStream.bufferedReader().use { it.readText() }
            ).jsonObject
        } finally {
            connection.disconnect()
        }
    }

    private const val TAG = "HomerunApi"
}
