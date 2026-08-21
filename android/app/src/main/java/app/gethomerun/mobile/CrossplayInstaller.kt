package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

/**
 * The half of crossplay that Modrinth cannot supply.
 *
 * # What crossplay needs, and which half is here
 *
 * A crossplay server is an ordinary Java server plus two plugins. **Geyser**
 * speaks the Bedrock protocol and translates it into a Java session;
 * **Floodgate** lets those sessions in without a Mojang account, so the server
 * keeps `online-mode=true` and Java players still authenticate normally.
 *
 * Geyser is on Modrinth for Paper, so [ModInstaller] fetches it like any other
 * plugin — `minecraft::crossplay::merge_projects` folds the slug into the
 * server's list before the resolver ever runs. **Floodgate is not**: its
 * Modrinth listing publishes fabric and neoforge builds only, and asking that
 * resolver for a Paper build returns nothing and installs nothing without
 * complaining. So the Bukkit-family jar comes from GeyserMC's own download API,
 * which is what this file is for.
 *
 * On Fabric both come from Modrinth and this whole file is a no-op. The core
 * decides which of those two worlds a server is in; nothing here does.
 *
 * # Why nothing here can fail a launch
 *
 * [PluginInstaller] draws the opposite line, and the difference is what the
 * jars are for. A minigame plugin *is* the server — a BedWars lobby with no
 * BedWars in it is not a server anyone asked for. Floodgate is not: without it
 * the Java server is still a working Java server on its usual address, and the
 * people who lose out are the Bedrock players who cannot join yet. Refusing to
 * start would take the game away from everybody to punish a download.
 *
 * So every failure is reported on the console the player is reading and the
 * launch continues. The same rule [ModInstaller] follows, for the same reason.
 */
object CrossplayInstaller {

    private const val TAG = "HomerunCrossplay"

    private const val CONNECT_TIMEOUT_MS = 30_000
    private const val READ_TIMEOUT_MS = 60_000

    private const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

    private val json = Json { ignoreUnknownKeys = true }

    /**
     * Put Floodgate in place and seed Geyser's config, for a crossplay server.
     *
     * A no-op for anything else, which is nearly every server, so this is safe
     * to call on every launch — and is.
     *
     * Never throws.
     */
    suspend fun sync(
        dir: File,
        loader: String,
        gameType: String,
        onLog: (String) -> Unit,
    ) = withContext(Dispatchers.IO) {
        val source = runCatching { Core.crossplayFloodgate(gameType, loader) }
            .onFailure { Log.w(TAG, "could not ask about floodgate: ${it.message}") }
            .getOrNull()

        if (source != null) {
            runCatching { installFloodgate(dir, source, onLog) }
                .onFailure {
                    Log.w(TAG, "floodgate install failed: ${it.message}")
                    onLog(
                        "[Homerun] Could not install Floodgate: ${it.message ?: "the download failed"}. " +
                            "Java players are unaffected; Bedrock players may not be able to join."
                    )
                }
        }

        runCatching { seedConfig(dir, loader, gameType, onLog) }
            .onFailure { Log.w(TAG, "geyser config seed failed: ${it.message}") }
    }

    /**
     * Fetch the newest Floodgate build named by GeyserMC's metadata.
     *
     * Two requests, not one: the metadata names the build **and** carries a
     * SHA-256 and the canonical filename for it, so the download can be
     * verified and the destination is GeyserMC's name rather than ours.
     */
    private fun installFloodgate(dir: File, source: Core.FloodgateSource, onLog: (String) -> Unit) {
        val meta = fetchJson(source.metaUrl)
        val fetch = Core.crossplayFloodgateBuild(meta, source.flavour)
            ?: throw IOException("the build metadata named no ${source.flavour} download")

        val subDir = File(dir, fetch.subDir).apply { mkdirs() }
        val dest = File(subDir, fetch.fileName)

        // Already the build the metadata names, so the launch pays nothing. The
        // check is the digest rather than mere existence: this file is replaced
        // in place under a stable name, so "there is a jar called this" says
        // nothing at all about which build it holds.
        if (fetch.sha256 != null && dest.isFile && digest(dest) == fetch.sha256.lowercase()) {
            Log.i(TAG, "floodgate already current")
            return
        }

        onLog("[Homerun] Installing Floodgate for Bedrock players...")
        download(fetch.url, dest, fetch.sha256)
        onLog("[Homerun] Installed ${fetch.fileName}.")
    }

    /**
     * Write Geyser's configuration, **only when it is not already there**.
     *
     * Geyser rewrites this file with every default expanded the first time it
     * starts. Dropping our two-key partial back over that on the next launch
     * would hand Geyser a config with no `config-version` and invite a
     * migration nobody has tested — and there is nothing to correct anyway,
     * because nothing on a phone can edit the file in between.
     *
     * See `minecraft::crossplay::config` for what is in it and why those two
     * keys and no others.
     */
    private fun seedConfig(dir: File, loader: String, gameType: String, onLog: (String) -> Unit) {
        val file = Core.crossplayConfig(gameType, loader) ?: run {
            // Only reachable for a crossplay server on a loader whose Geyser
            // directory the core has not been taught. Said out loud because the
            // symptom otherwise is Bedrock players being asked to sign in with
            // a Java account, which reads as anything but a missing file.
            if (Core.isCrossplay(gameType)) {
                onLog("[Homerun] No Geyser configuration is known for a $loader server.")
            }
            return
        }

        val dest = File(dir, file.path)
        if (dest.isFile) return

        dest.parentFile?.mkdirs()
        dest.writeText(file.contents, file.encoding.charset)
        Log.i(TAG, "seeded ${file.path}")
        onLog("[Homerun] Configured Geyser for Bedrock players.")
    }

    /**
     * Fetch to a temporary file, verify, and move it into place.
     *
     * The same precaution [PluginInstaller] and [ServerJar] take, and the same
     * reason: a half-written jar under the right name is worse than no jar,
     * because Bukkit reads it, fails, and the launch looks like a plugin bug.
     */
    private fun download(url: String, dest: File, sha256: String?) {
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

        if (sha256 != null) {
            val actual = digest(part)
            if (!actual.equals(sha256, ignoreCase = true)) {
                part.delete()
                throw IOException("checksum mismatch: expected $sha256, got $actual")
            }
        }

        // `renameTo` will not replace an existing file on every filesystem, and
        // this path exists to replace one — the name is stable across builds so
        // that an update never leaves the previous jar beside the new one.
        dest.delete()
        if (!part.renameTo(dest)) {
            part.delete()
            throw IOException("could not move ${dest.name} into place")
        }
    }

    private fun digest(file: File): String {
        val md = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buffer = ByteArray(64 * 1024)
            while (true) {
                val read = input.read(buffer)
                if (read < 0) break
                md.update(buffer, 0, read)
            }
        }
        return md.digest().joinToString("") { "%02x".format(it) }
    }

    private fun fetchJson(url: String): JsonElement {
        val connection = open(url)
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode} from $url")
            }
            return json.parseToJsonElement(connection.inputStream.bufferedReader().readText())
        } finally {
            connection.disconnect()
        }
    }

    private fun open(url: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
            instanceFollowRedirects = true
        }
}
