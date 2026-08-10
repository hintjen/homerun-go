package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.add
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonArray
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

    private val json = Json { ignoreUnknownKeys = true }

    /**
     * Read a server's settings, or null if they could not be read.
     *
     * Null is a normal outcome, not an error: no token yet, no signal, a
     * backend hiccup. The caller falls back to vanilla-latest exactly as the
     * desktop does — refusing to start a server because a settings lookup
     * failed would be worse than starting the default one.
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

            val env = body["config"]?.jsonObject?.get("environment_variables")?.jsonObject
            val type = env?.get("TYPE")?.jsonPrimitive?.contentOrNull?.lowercase()
            val gameType = (
                body["game_type"]?.jsonPrimitive?.contentOrNull
                    ?: body["config"]?.jsonObject?.get("game_type")?.jsonPrimitive?.contentOrNull
                ).orEmpty()

            ServerSettings(
                version = env?.get("VERSION")?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() },
                loader = if (type in LOADERS) type!! else "vanilla",
                gameType = if (gameType == "bedrock" || gameType == "native-bedrock") "bedrock" else "java",
                rawGameType = gameType,
                env = env ?: JsonObject(emptyMap()),
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

    /** A link plus the one field that changes how staleness is judged. */
    private data class PolledLink(val link: WireProxy.Link, val isGateway2: Boolean)

    /** The current `native_config`, or null when the gateway has not written one. */
    private fun readLink(apiUrl: String, serverId: String, token: String): PolledLink? {
        val body = get(apiUrl, "/api/server/$serverId/", token) ?: return null
        return linkOf(body)
    }

    /** Pull the tunnel out of a `/api/server/<id>/` body. */
    private fun linkOf(body: JsonObject): PolledLink? {
        val link = body["config"]?.jsonObject?.get("links")?.jsonArray
            ?.firstOrNull()?.jsonObject ?: return null
        val native = link["native_config"]?.takeIf { it !is JsonNull }?.jsonObject ?: return null

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
     * **user** token — owner, matrix id and device type are all derived
     * server-side, so the client sends only a name.
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
     * Acknowledge a server's state, with the **device** token. This is the
     * report the API and the web dashboard wait on — the bridge event of the
     * same name only reaches the page in front of us.
     */
    suspend fun reportServerState(
        apiUrl: String,
        serverId: String,
        state: String,
        deviceToken: String,
    ) = withContext(Dispatchers.IO) {
        val body = buildJsonObject { put("status", state) }
        runCatching { post(apiUrl, "/api/server/$serverId/state/", body, deviceToken) }
            .onFailure { Log.w(TAG, "state report ($state) failed for $serverId: ${it.message}") }
        Unit
    }

    /** One authenticated POST, parsed. Throws on a non-2xx. */
    private fun post(
        apiUrl: String,
        path: String,
        body: JsonObject,
        token: String,
    ): JsonObject? {
        val connection = (URL("${apiUrl.trimEnd('/')}$path").openConnection() as HttpURLConnection)
            .apply {
                requestMethod = "POST"
                doOutput = true
                connectTimeout = CONNECT_TIMEOUT_MS
                readTimeout = READ_TIMEOUT_MS
                setRequestProperty("Authorization", "Bearer $token")
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
