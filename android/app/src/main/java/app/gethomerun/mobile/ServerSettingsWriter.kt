package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone

/**
 * Writes a server's config files before it starts.
 *
 * # Why this runs on every launch
 *
 * A server reads its config once, before it accepts anyone. Writing on every
 * launch is what makes a change made in the creation wizard or on the web
 * dashboard take effect — and, more importantly, what makes a *removal* take
 * effect. An add-only pass over RCON (which the desktop used to do) leaves a
 * de-opped player an operator forever.
 *
 * Until this existed, Android wrote nothing, and every server silently ran
 * vanilla defaults no matter what it had been told.
 *
 * # This file does not know what Minecraft is
 *
 * It asks the core three questions — *which files should I read*, *whose
 * identity must I fetch*, *what should I write* — and does as it is told. The
 * property keys, the merge, the UUID derivation, the ban semantics and the
 * per-file encoding all live in `homerun-core`, behind the `Game` trait, so
 * this app and the desktop and iOS cannot drift and a second game needs no
 * change here at all.
 *
 * What is left is what only a host can do: touch the filesystem and the
 * network.
 */
object ServerSettingsWriter {

    private const val TAG = "HomerunSettings"

    /** The timestamp format a Minecraft ban record carries. */
    private val TIMESTAMP = SimpleDateFormat("yyyy-MM-dd HH:mm:ss Z", Locale.US)
        .apply { timeZone = TimeZone.getTimeZone("UTC") }

    private const val MOJANG_PROFILE = "https://api.mojang.com/users/profiles/minecraft/"
    private const val PROFILE_TIMEOUT_MS = 10_000

    private val parser = Json { ignoreUnknownKeys = true }

    /**
     * Write every config file for a launch.
     *
     * Never throws: a config write that fails should not stop a server that
     * would otherwise run. Failures are logged and surfaced through [onLog],
     * because a player whose ops did not apply deserves to see why.
     *
     * @param env `environment_variables` as the API returned it
     * @param gameType the API's game type verbatim — crossplay changes the answer
     * @param port the port the server will actually bind
     */
    suspend fun apply(
        serverId: String,
        dir: File,
        env: JsonObject,
        gameType: String,
        port: Int,
        onLog: (String) -> Unit,
    ) {
        try {
            // 1. Read what the core asks for, with the encoding it asks for.
            val existing = Core.configInputs(env)
                .mapNotNull { input ->
                    val file = File(dir, input.path)
                    if (!file.exists()) return@mapNotNull null
                    runCatching { input.path to file.readText(input.encoding.charset) }.getOrNull()
                }
                .toMap()

            // 2. Fetch only the identities the core cannot derive itself. An
            //    offline server asks for none, so this costs nothing there.
            val resolved = Core.requiredLookups(env, gameType).mapNotNull { name ->
                val id = fetchIdentity(name)
                if (id == null) {
                    onLog("[Homerun] Could not resolve \"$name\" — skipping.")
                    null
                } else {
                    Core.Identity(name, id)
                }
            }

            // 3. Write what it says, how it says.
            val files = Core.configFiles(
                env = env,
                gameType = gameType,
                port = port,
                // Loopback only. Players reach this server through the gateway
                // tunnel, and a phone on a shared network has no business
                // listening on every interface by default.
                bindAddress = "127.0.0.1",
                existing = existing,
                resolved = resolved,
                now = TIMESTAMP.format(Date()),
            )

            for (file in files) {
                runCatching { File(dir, file.path).writeText(file.contents, file.encoding.charset) }
                    .onFailure { Log.w(TAG, "$serverId: could not write ${file.path}: ${it.message}") }
            }
            Log.i(TAG, "$serverId: wrote ${files.joinToString { it.path }}")
        } catch (err: Exception) {
            // Including CoreException. The server's own defaults are a better
            // outcome than refusing to start.
            Log.w(TAG, "$serverId: could not apply settings: ${err.message}")
            onLog("[Homerun] Server settings could not be applied, so the server's defaults apply.")
        }
    }

    /**
     * Look a player up with Mojang.
     *
     * Null on any failure — unknown name, rate limit, no signal — so the core
     * can leave that entry out rather than write an id that will never match.
     */
    private suspend fun fetchIdentity(name: String): String? = withContext(Dispatchers.IO) {
        runCatching {
            val url = URL(MOJANG_PROFILE + URLEncoder.encode(name, "UTF-8"))
            val connection = (url.openConnection() as HttpURLConnection).apply {
                connectTimeout = PROFILE_TIMEOUT_MS
                readTimeout = PROFILE_TIMEOUT_MS
                setRequestProperty("Accept", "application/json")
            }
            try {
                // 204 and 404 both mean "no such player" — not an error worth
                // a stack trace, just a name that cannot be written.
                if (connection.responseCode != 200) return@runCatching null
                val body = connection.inputStream.bufferedReader().readText()
                val id = parser.parseToJsonElement(body).jsonObject["id"]
                    ?.jsonPrimitive?.contentOrNull
                    ?: return@runCatching null
                Core.dashUuid(id)
            } finally {
                connection.disconnect()
            }
        }.getOrNull()
    }
}
