package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicBoolean
import java.util.zip.ZipInputStream
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Fetching a new UI bundle. The half that goes and looks.
 *
 * [BundleStore] is the half that decides what to serve and rolls back what does
 * not work; this one only ever produces a `pending` directory for it. The split
 * matters: everything here is best-effort and can fail silently, because
 * nothing here is on the path to showing the user a screen.
 *
 * # Nothing here decides anything
 *
 * The manifest's signature, whether this host may run the bundle, and whether
 * it is newer than what is installed are all answered by
 * `homerun_core::bundle`, in one call, so Android and iOS cannot judge the same
 * manifest differently. What is left in Kotlin is the part only Kotlin can do:
 * the request, the digest of a file on disk, the unzip, the rename.
 *
 * # The order things happen in
 *
 * 1. Throttle — most launches do nothing at all.
 * 2. Ask the API, with the device token, for what this host should be running.
 * 3. Hand the reply *verbatim* to the core along with the compiled-in public
 *    key. Nothing below happens unless it comes back `install`.
 * 4. Download to the cache, hashing as the bytes stream past.
 * 5. Compare that digest with the signed one, in the core.
 * 6. Unpack into a staging directory, then let [BundleStore] rename it.
 *
 * A failure at any step leaves the app exactly as it was. That is the whole
 * design: the worst outcome of this class never running is an app that is a
 * few days out of date.
 *
 * # Why the check is not on the critical path
 *
 * It runs on [Dispatchers.IO] from a scope tied to the activity, after the page
 * has been asked to load. `plans/ota-updates.md`: "Checks run at launch and on
 * resume, throttled, and never block startup. An update that has not finished
 * downloading is simply not applied yet."
 *
 * What happens *after* it finishes downloading is the activity's call, not
 * this one's: [onBundleStaged] fires and `MainActivity.applyStagedBundle`
 * decides whether the app can take it now. See `docs/ota-bundles.md`.
 */
object BundleUpdater {

    private const val TAG = "HomerunUpdate"

    /**
     * How long after a check before another is worth making.
     *
     * Six hours, and the ceiling on how stale a device can be is this plus one
     * launch. Shorter would mean a request every time the user switches back to
     * the app, for a bundle that changes a few times a week at most.
     */
    private const val THROTTLE_MS = 6 * 60 * 60 * 1000L

    /**
     * Refuse an archive bigger than this.
     *
     * A bundle is ~3.5 MB (`plans/ota-updates.md`); 64 MB is far above any real
     * one and far below "fills the user's phone". The digest would catch a
     * substituted archive, but only *after* it had been written to disk, and a
     * device with no storage left is a worse failure than a missed update.
     */
    private const val MAX_ARCHIVE_BYTES = 64L * 1024 * 1024

    /** The same, for what an archive expands to. Zip bombs are cheap to make. */
    private const val MAX_UNPACKED_BYTES = 256L * 1024 * 1024
    private const val MAX_ENTRIES = 10_000

    private const val CONNECT_TIMEOUT_MS = 15_000
    private const val READ_TIMEOUT_MS = 60_000

    private const val PREFS = "bundle-updater"
    private const val LAST_CHECK = "lastCheckMs"

    /**
     * One check at a time.
     *
     * `onCreate` and `onResume` both call [check], and on a cold start both run
     * within a second of each other. Without this the same archive downloads
     * twice and the two unpack into one staging directory.
     */
    private val checking = AtomicBoolean(false)

    /**
     * Called on the IO thread when a bundle becomes `pending`.
     *
     * This is what turns a silent background download into a UI the user is
     * looking at: `MainActivity` wires it to `applyStagedBundle`, which puts
     * the bundle on screen there and then unless the device is mid-call or
     * hosting. Nothing is offered and nothing is asked.
     *
     * A callback rather than a direct apply because this object has no page,
     * no activity and no WebView, and must keep working when none of them
     * exist.
     */
    @Volatile
    var onBundleStaged: ((String) -> Unit)? = null

    /**
     * Ask, if it is time to ask. Returns immediately; everything happens later.
     *
     * @param force skip the throttle. For the debug-only manual trigger; a
     *   user-visible "check now" button would use it too.
     */
    fun check(context: Context, scope: CoroutineScope, force: Boolean = false) {
        val app = context.applicationContext
        scope.launch(Dispatchers.IO) {
            runCatching { checkNow(app, force) }
                .onFailure {
                    // Deliberately not rethrown. An update check that throws
                    // must not be able to take the activity's scope with it.
                    Log.w(TAG, "update check failed: ${it.message}")
                }
        }
    }

    /**
     * Check now and answer when it is done: the id of the bundle waiting to go
     * live, or null.
     *
     * Backs `wait-for-update-check`, which is an **invoke** — so this must
     * always return. Every failure inside is swallowed into null for that
     * reason: an update check that cannot reach the network is not a reason to
     * leave a UI promise unresolved, and an unanswered invoke hangs for ever.
     *
     * Reports what is *staged*, not what was downloaded on this call, so a
     * bundle fetched by an earlier background check is still offered.
     */
    suspend fun awaitCheck(context: Context, force: Boolean = true): String? =
        withContext(Dispatchers.IO) {
            val app = context.applicationContext
            runCatching { checkNow(app, force) }
                .onFailure { Log.w(TAG, "update check failed: ${it.message}") }
            runCatching { BundleStore.pending(app) }.getOrNull()
        }

    private fun checkNow(context: Context, force: Boolean) {
        if (!BuildConfig.OTA_UPDATES) {
            // A development build that wants to keep the UI it was built with.
            // Info rather than debug: this is a deliberate build flag, and the
            // one question it will be asked is "why is my phone not updating".
            Log.i(TAG, "over-the-air updates are off in this build (-PotaUpdates=off)")
            return
        }
        if (BuildConfig.BUNDLE_PUBLIC_KEY.isBlank()) {
            // No key compiled in means no way to tell a real manifest from any
            // other. The only safe behaviour is to do nothing at all — never to
            // fetch and trust. This is the state of a build made before the
            // signing key exists, so it is a debug log rather than a warning.
            Log.d(TAG, "no bundle signing key in this build; over-the-air updates are off")
            return
        }
        if (!checking.compareAndSet(false, true)) return
        try {
            val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val since = System.currentTimeMillis() - prefs.getLong(LAST_CHECK, 0L)
            if (!force && since < THROTTLE_MS) return

            val device = DeviceRegistry.current()
            if (device == null) {
                // Before the user has signed in there is nobody to ask on
                // behalf of. Not an error, and not worth a timestamp either —
                // the check should happen promptly once they do.
                Log.d(TAG, "no device registration yet; not checking for a bundle")
                return
            }

            // Written before the work, not after: a check that fails slowly
            // must not be retried on every resume.
            prefs.edit().putLong(LAST_CHECK, System.currentTimeMillis()).apply()

            val manifestJson = fetchManifest(device.deviceToken) ?: return
            val offer = runCatching {
                Core.evaluateBundle(manifestJson, BuildConfig.BUNDLE_PUBLIC_KEY, BundleStore.installed())
            }.getOrElse {
                // A signature that does not verify is the one failure here that
                // is not routine. Everything else is "no new bundle".
                Log.e(TAG, "refusing the manifest: ${it.message}")
                return
            }

            if (!offer.install) {
                Log.i(TAG, "no update: ${offer.reason}")
                return
            }
            if (BundleStore.pending(context) == offer.bundle) {
                // Announced once, when it was staged. If it is still here, the
                // activity is holding it back for a reason of its own and will
                // take it at the next idle moment or the next launch.
                Log.i(TAG, "bundle ${offer.bundle} is already staged and waiting to go live")
                return
            }

            Log.i(TAG, "fetching bundle ${offer.bundle} from ${offer.url}")
            install(context, offer)
        } finally {
            checking.set(false)
        }
    }

    // -----------------------------------------------------------------------
    // The request
    // -----------------------------------------------------------------------

    /**
     * The manifest, as text.
     *
     * Returned unparsed on purpose. Parsing it here would mean a `Manifest`
     * existed in Kotlin before anything had checked its signature, and the
     * first person to use one of its fields would have introduced a hole with
     * no symptom. The core takes the raw string.
     */
    private fun fetchManifest(deviceToken: String): String? {
        val query = listOf(
            "platform=${BundleStore.PLATFORM}",
            "host=${BridgeRouter.HOST_REVISION}",
            "app=${BuildConfig.VERSION_NAME}",
            "channel=$CHANNEL",
        ).joinToString("&")
        val url = URL("${BuildConfig.API_URL.trimEnd('/')}/api/mobile/bundle/?$query")

        val connection = (url.openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            // The device token, not the user's. This is a property of the
            // install, and the repo's rule is that reporting-shaped traffic is
            // signed with the device.
            setRequestProperty("Authorization", "Bearer $deviceToken")
            setRequestProperty("Accept", "application/json")
        }
        try {
            // 204 is how the server says "nothing for you" — a channel with no
            // release, or a rollout this device is not in yet.
            if (connection.responseCode == HttpURLConnection.HTTP_NO_CONTENT) {
                Log.i(TAG, "the server has no bundle for this host")
                return null
            }
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                // The URL belongs in the message. "HTTP 404 asking for a
                // bundle" is unactionable — a wrong API_URL, an undeployed
                // endpoint and a typo'd path all look identical, and the one
                // fact that separates them is the one the message omitted.
                throw IOException("HTTP ${connection.responseCode} from $url")
            }
            return connection.inputStream.bufferedReader().use { it.readText() }
        } finally {
            connection.disconnect()
        }
    }

    // -----------------------------------------------------------------------
    // The download
    // -----------------------------------------------------------------------

    private fun install(context: Context, offer: Core.Offer) {
        val archive = File(context.cacheDir, "ui-bundle.zip")
        archive.delete()
        try {
            val digest = download(offer.url, archive)
            if (!Core.digestMatches(offer.sha256, digest)) {
                // The signed digest and the delivered bytes disagree. This is
                // the check the whole signature exists to enable, so say so at
                // error level: a truncated download and a substituted archive
                // look identical here, and both are worth seeing.
                Log.e(TAG, "bundle ${offer.bundle} does not match its signed digest; discarding it")
                return
            }

            val staging = BundleStore.stagingDir(context)
            unpack(archive, staging)
            if (BundleStore.stage(context, staging, offer.bundle, offer.minHost, offer.serial)) {
                // Outside the store on purpose: staging is a disk fact, telling
                // the user about it is a page fact, and the store has no page.
                runCatching { onBundleStaged?.invoke(offer.bundle) }
                    .onFailure { Log.w(TAG, "could not announce ${offer.bundle}: ${it.message}") }
            }
        } finally {
            // Several megabytes in the cache directory that nothing will ever
            // read again. Android would eventually reclaim it; doing it here
            // means it is gone in the failure cases too.
            archive.delete()
        }
    }

    /** Stream the archive to [into], returning its SHA-256 as lowercase hex. */
    private fun download(from: String, into: File): String {
        val connection = (URL(from).openConnection() as HttpURLConnection).apply {
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            // No Authorization header: the CDN is public and signed for, and
            // sending the device token to a host outside our API would leak it
            // to whoever the manifest named.
            setRequestProperty("Accept", "application/zip")
        }
        try {
            if (connection.responseCode != HttpURLConnection.HTTP_OK) {
                throw IOException("HTTP ${connection.responseCode} fetching the bundle")
            }
            val sha = MessageDigest.getInstance("SHA-256")
            var total = 0L
            connection.inputStream.use { source ->
                into.outputStream().use { sink ->
                    val buffer = ByteArray(64 * 1024)
                    while (true) {
                        val read = source.read(buffer)
                        if (read < 0) break
                        total += read
                        if (total > MAX_ARCHIVE_BYTES) {
                            throw IOException("the bundle archive is larger than $MAX_ARCHIVE_BYTES bytes")
                        }
                        sha.update(buffer, 0, read)
                        sink.write(buffer, 0, read)
                    }
                }
            }
            return sha.digest().joinToString("") { "%02x".format(it) }
        } finally {
            connection.disconnect()
        }
    }

    // -----------------------------------------------------------------------
    // The unpack
    // -----------------------------------------------------------------------

    /**
     * Expand [archive] into [into], refusing anything that tries to leave it.
     *
     * The digest proves the archive is the one that was signed; it says nothing
     * about whether the archive is *well behaved*. An entry named
     * `../../databases/homerun.db` is a valid zip entry, and `File(into, name)`
     * resolves it happily — that is Zip Slip, and it is a file write anywhere
     * this process can reach. The canonical-path check below is the fix, and it
     * is the same one `WebViewAssetLoader.InternalStoragePathHandler` performs
     * when serving out of this directory later.
     *
     * The entry and size ceilings are for the other shape of hostile archive:
     * one that is small enough to sign and expands until the device is full.
     */
    private fun unpack(archive: File, into: File) {
        val root = into.canonicalFile
        var entries = 0
        var written = 0L

        ZipInputStream(archive.inputStream().buffered()).use { zip ->
            while (true) {
                val entry = zip.nextEntry ?: break
                if (++entries > MAX_ENTRIES) throw IOException("the bundle has more than $MAX_ENTRIES entries")

                val target = File(root, entry.name).canonicalFile
                if (!target.path.startsWith(root.path + File.separator)) {
                    throw IOException("the bundle contains an entry outside itself: ${entry.name}")
                }

                if (entry.isDirectory) {
                    target.mkdirs()
                } else {
                    target.parentFile?.mkdirs()
                    written += copyCapped(zip, target, MAX_UNPACKED_BYTES - written)
                }
                zip.closeEntry()
            }
        }
    }

    /** Copy one entry, refusing to write more than [remaining] bytes. */
    private fun copyCapped(source: InputStream, target: File, remaining: Long): Long {
        if (remaining <= 0) throw IOException("the bundle expands to more than $MAX_UNPACKED_BYTES bytes")
        var written = 0L
        target.outputStream().use { sink ->
            val buffer = ByteArray(64 * 1024)
            while (true) {
                val read = source.read(buffer)
                if (read < 0) break
                written += read
                if (written > remaining) {
                    throw IOException("the bundle expands to more than $MAX_UNPACKED_BYTES bytes")
                }
                sink.write(buffer, 0, read)
            }
        }
        return written
    }

    /**
     * Which release track this build follows.
     *
     * A constant until there is something to switch: the server decides rollout
     * within a channel, so a second channel only earns its place when someone
     * needs to be on one deliberately.
     */
    private const val CHANNEL = "stable"
}
