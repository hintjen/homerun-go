package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.contentOrNull
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

        val url = "${apiUrl.trimEnd('/')}/api/server/$serverId/"
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("Authorization", "Bearer $token")
            setRequestProperty("Accept", "application/json")
        }

        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            val body = json.parseToJsonElement(
                connection.inputStream.bufferedReader().use { it.readText() }
            ).jsonObject

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
            ).also { Log.i(TAG, "$serverId: ${it.loader} ${it.version ?: "latest"} (${it.gameType})") }
        } catch (err: Exception) {
            Log.w(TAG, "could not read settings for $serverId: ${err.message}")
            null
        } finally {
            connection.disconnect()
        }
    }

    private const val TAG = "HomerunApi"
}
