package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
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
