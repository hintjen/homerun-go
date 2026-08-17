package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonObject
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * Putting Homerun's own plugin jars onto the device.
 *
 * # What this is, and what [ModInstaller] is
 *
 * [ModInstaller] fetches what a player picked from Modrinth. This fetches what
 * *we* wrote — the minigame framework, the BedWars fork, the lobby autopilot —
 * which are not on Modrinth and which no resolver will ever find. They arrive
 * as URLs in the server's `CUSTOM_PLUGINS`, put there when the server was
 * created from a template in the Games browser.
 *
 * The split matters for one concrete reason: `mods::sweep` deletes jars it
 * does not recognise as its own, so these have to land *after* it has run and
 * be invisible to it. They are — nothing here writes a record it reads.
 *
 * # Why every launch re-fetches
 *
 * Each URL is a **stable resolve endpoint** that redirects to the newest jar
 * on this server's release channel. Fetching it again is the entire update
 * mechanism: a plugin fix reaches every server on the next start, and nobody
 * has to recreate anything. The filename is derived from the URL rather than
 * the redirect target, so the new jar replaces the old one instead of joining
 * it — see `minecraft::minigame::custom_plugins`, which is also where the
 * filename is decided and why a host must not decide it locally.
 *
 * # Why a failure here can stop a launch, when a mod failing never does
 *
 * A mod is decoration on a world that exists without it. These jars *are* the
 * server: without them a BedWars lobby is an empty Paper world with a player
 * standing in it wondering what went wrong. The desktop draws the same line —
 * `downloadCustomPlugins` has no per-URL catch and a failure fails the start.
 *
 * With one deliberate softening for this platform. If the jar is already on
 * disk from a previous launch and only the *refresh* failed, this starts the
 * server on what it has rather than refusing: a phone loses its connection for
 * reasons a PC does not, and an older build of a working plugin beats a game
 * that will not start. A jar that is missing entirely is still fatal, because
 * there is no game to run.
 */
object PluginInstaller {

    private const val CONNECT_TIMEOUT_MS = 30_000
    private const val READ_TIMEOUT_MS = 60_000

    private const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

    /** Where Bukkit looks. Not [Core.modsSubDir]'s answer — this is never `mods/`. */
    private const val PLUGINS = "plugins"

    /**
     * Fetch this server's Homerun-hosted plugins into `plugins/`.
     *
     * A no-op for a server with none configured, and for any loader that would
     * not read the directory — so this is safe to call on every launch, which
     * is how it is called.
     *
     * @throws ServerBackendException.Engine when a plugin this server needs
     *   could not be fetched and is not already on disk.
     */
    suspend fun sync(
        dir: File,
        loader: String,
        env: JsonObject?,
        onLog: (String) -> Unit,
    ) = withContext(Dispatchers.IO) {
        val plugins = Core.customPlugins(loader, env)
        if (plugins.isEmpty()) return@withContext

        val pluginsDir = File(dir, PLUGINS).apply { mkdirs() }

        onLog("[Homerun] Installing minigame plugins...")

        val installed = mutableListOf<String>()
        for (plugin in plugins) {
            val dest = File(pluginsDir, plugin.filename)
            try {
                download(plugin.url, dest)
                installed += plugin.filename
            } catch (err: Exception) {
                // `length() > 0` rather than `exists()`: a truncated file from
                // an interrupted run is not something to start a game on.
                if (dest.length() > 0) {
                    Log.w(TAG, "could not refresh ${plugin.filename}: ${err.message}")
                    onLog(
                        "[Homerun] Could not check ${plugin.filename} for updates — " +
                            "using the copy already installed."
                    )
                    continue
                }
                Log.e(TAG, "could not install ${plugin.filename}: ${err.message}")
                throw ServerBackendException.Engine(
                    "This game's plugins could not be downloaded, so the server " +
                        "would have started without the game. Check your connection " +
                        "and try again."
                )
            }
        }

        if (installed.isNotEmpty()) {
            onLog("[Homerun] Installed ${installed.size} minigame plugin(s): ${installed.joinToString()}")
        }
    }

    /**
     * Fetch to a temporary file and move it into place.
     *
     * Same precaution as [ModInstaller] and `ServerJar`, and the same reason: a
     * half-written jar with the right name is worse than no jar, because Bukkit
     * reads it, fails, and the launch that follows looks like a plugin bug.
     */
    private fun download(url: String, dest: File) {
        val part = File(dest.parentFile, "${dest.name}.part")
        val connection = open(url)
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode}")
            }
            connection.inputStream.use { input ->
                part.outputStream().use { input.copyTo(it) }
            }
        } catch (err: Exception) {
            part.delete()
            throw err
        } finally {
            connection.disconnect()
        }
        // `renameTo` will not replace an existing file on every filesystem, and
        // this path exists to replace one on every single launch.
        dest.delete()
        if (!part.renameTo(dest)) {
            part.delete()
            throw IOException("could not move ${dest.name} into place")
        }
    }

    private fun open(url: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            // Follows the resolve endpoint's 302 to the channel's newest jar.
            instanceFollowRedirects = true
            setRequestProperty("User-Agent", USER_AGENT)
        }

    private const val TAG = "HomerunPlugins"
}
