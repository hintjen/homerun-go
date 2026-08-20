package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import java.io.File
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put

/**
 * Which UI bundle the WebView is served from, and what happens when a new one
 * turns out to be broken.
 *
 * See `plans/ota-updates.md`. The short version: every screen in this app is
 * the shared web bundle, both stores explicitly permit replacing it at runtime,
 * and doing so is the difference between shipping a fix in an hour and shipping
 * it in a week. This class is the part that makes that survivable.
 *
 * # The layout
 *
 * ```
 * files/ui/current     the bundle being served
 * files/ui/previous    the last one known to have reached __bridge:ready
 * files/ui/pending     downloaded and verified, not yet live
 * files/ui/probation   how many launches the current bundle has left to prove itself
 * (app assets)         the floor — never deleted, never overwritten
 * ```
 *
 * A bundle directory **is** the web root: `index.html` sits at the top of it,
 * next to a [MANIFEST] naming the bundle and the host revision it needs. Both
 * are required. A directory with no manifest is not a bundle with an unknown
 * name, it is something we did not put there, and serving it would give us a
 * bundle we cannot identify in a bug report or match against a probation
 * record.
 *
 * # Why probation is on disk
 *
 * The failure this exists to survive is a bundle that kills the app before the
 * page can say anything — a syntax error in a chunk, an API the WebView on one
 * OEM's build does not have. An in-memory counter dies with the process without
 * ever recording that the attempt happened, so a fatal bundle would retry for
 * ever and the app would be bricked in a way **no store update could fix**,
 * because the broken bundle outranks the one in the binary.
 *
 * So the counter is decremented at [resolve], before the page is given a chance
 * to crash, and cleared at [confirm] when the handshake proves it did not.
 *
 * # Not a hot swap
 *
 * [activate] runs before the WebView is built and never while one is live.
 * Swapping the bundle under a running page would cancel whatever bridge call is
 * in flight — and `native-server-start` runs for minutes.
 */
object BundleStore {

    /** What [resolve] settled on, and what [WebBundle] should serve. */
    data class Loaded(
        /** The bundle id, or [SHIPPED] for the copy inside the APK. */
        val id: String,
        /** The directory to serve, or null to serve `assets/web/`. */
        val root: File?,
        /**
         * The release ordering this bundle came from; `0` for the shipped copy,
         * which every real release outranks.
         *
         * Separate from [id] because ids are dates and dates do not order
         * reliably across channels or re-cuts. `homerun_core::bundle` refuses
         * anything whose serial is not strictly greater than this, which is
         * what stops a replayed old manifest from rolling every device back to
         * a version whose bugs an attacker already knows.
         */
        val serial: Long = 0,
    )

    /**
     * The id reported for the bundle compiled into the app.
     *
     * A name rather than null deliberately: this shows up in `get-app-version`
     * and therefore in bug reports, and "shipped" is an answer where null reads
     * as "the host did not say".
     */
    const val SHIPPED = "shipped"

    /**
     * Launches a freshly activated bundle gets to reach `__bridge:ready` before
     * it is judged broken. Two, not one: the first launch after an update is
     * also the launch most likely to be killed for reasons that are not the
     * bundle's fault — a low-memory kill while a server is starting, the user
     * swiping the app away mid-splash.
     */
    private const val ATTEMPTS = 2

    private const val UI = "ui"
    private const val CURRENT = "current"
    private const val PREVIOUS = "previous"
    private const val PENDING = "pending"
    private const val PROBATION = "probation"
    private const val MANIFEST = "bundle.json"

    /**
     * Where [BundleUpdater] unpacks an archive before it is a bundle.
     *
     * Inside `ui/` rather than the cache directory so the final step is a
     * rename within one filesystem — the atomic move that means `pending` never
     * names a half-unpacked tree. The leading dot keeps it out of the way of
     * the three real names.
     */
    private const val STAGING = ".staging"

    private const val TAG = "HomerunBundle"

    private val json = Json { ignoreUnknownKeys = true }

    /** What [resolve] last decided, for [active] to report without touching disk. */
    @Volatile
    private var loaded: Loaded? = null

    /** Whether the served bundle is still proving itself; keeps [confirm] free. */
    @Volatile
    private var onProbation = false

    /** What is being served. [SHIPPED] until [resolve] has run. */
    fun active(): String = loaded?.id ?: SHIPPED

    /**
     * Whether this build has anything to do with over-the-air bundles at all.
     *
     * `-PotaUpdates=off` (see `android/app/build.gradle.kts`) is for a
     * development build whose whole point is the UI that was just staged into
     * `assets/web/`. It means **ignore them entirely**, not merely "do not
     * fetch": [activate] promotes nothing, [resolve] serves the shipped copy,
     * and [pending] answers null so nothing downstream offers or applies one.
     *
     * Nothing on disk is touched — not the bundles, not the probation record —
     * so the same device with the flag back on carries on exactly where it left
     * off. A release cannot be built this way; Gradle's `verifyReleaseConfig`
     * refuses.
     */
    private val enabled: Boolean get() = BuildConfig.OTA_UPDATES

    /**
     * What the update check tells `homerun_core::bundle` about this device.
     *
     * Built from what [resolve] settled on rather than re-read from disk, so it
     * describes the bundle actually running — which is the one whose serial a
     * newer release has to beat.
     */
    fun installed(): JsonObject {
        val live = loaded
        return buildJsonObject {
            // Null, not "shipped": core distinguishes "no over-the-air bundle"
            // from "a bundle called shipped", and only the former may be
            // replaced by serial 1.
            if (live != null && live.root != null) put("bundle", live.id) else put("bundle", JsonNull)
            put("serial", live?.serial ?: 0L)
            put("hostRevision", BridgeRouter.HOST_REVISION)
            put("platform", PLATFORM)
        }
    }

    /** The `platform` a manifest must name to be for this host. */
    const val PLATFORM = "android"

    // -----------------------------------------------------------------------
    // Promotion
    // -----------------------------------------------------------------------

    /**
     * Make a downloaded bundle live, if one is waiting.
     *
     * Call before the WebView is built and not otherwise — see the class note.
     * Cheap when there is nothing pending, which is almost every launch.
     */
    @Synchronized
    fun activate(context: Context) {
        if (!enabled) return
        val ui = uiDir(context)
        val pending = File(ui, PENDING)
        if (!pending.exists()) return

        val bundle = readManifest(pending)
        if (bundle == null) {
            // Verified before it was written, so this means the download was
            // interrupted or something else wrote here. Either way it is not
            // ours to serve, and leaving it would retry the same judgement
            // every launch.
            Log.w(TAG, "discarding an unusable pending bundle")
            pending.deleteRecursively()
            return
        }

        val outgoing = File(ui, CURRENT)
        File(ui, PREVIOUS).deleteRecursively()
        if (outgoing.exists() && !outgoing.renameTo(File(ui, PREVIOUS))) {
            // Without a rollback target this update is one-way, so do not take
            // it. The pending bundle keeps its name and will be tried again.
            Log.e(TAG, "could not move the live bundle aside; staying on ${readManifest(outgoing)?.id}")
            return
        }
        if (!pending.renameTo(outgoing)) {
            // A rename within one directory failing is close to impossible, but
            // the state it would leave — no `current` — is one `resolve` has to
            // handle anyway, so say so loudly and let it.
            Log.e(TAG, "could not promote the pending bundle")
            return
        }

        writeProbation(ui, bundle.id, ATTEMPTS)
        Log.i(TAG, "activated bundle ${bundle.id}; $ATTEMPTS launches to prove itself")
    }

    // -----------------------------------------------------------------------
    // Staging — what [BundleUpdater] hands back
    // -----------------------------------------------------------------------

    /**
     * An empty directory to unpack an archive into.
     *
     * Cleared each time. A leftover tree from an interrupted download would
     * otherwise be unpacked *over*, mixing two bundles' files — the exact
     * failure the whole four-directory layout exists to prevent.
     */
    @Synchronized
    fun stagingDir(context: Context): File {
        val staging = File(uiDir(context), STAGING)
        staging.deleteRecursively()
        staging.mkdirs()
        return staging
    }

    /**
     * Make an unpacked tree the pending bundle. Returns false if it could not.
     *
     * The manifest is written **here**, from the signed values the update check
     * verified, rather than trusted from inside the archive. The archive's own
     * `bundle.json` is covered by the signed digest and so is not untrusted
     * exactly — but it is a second copy of facts that already have an
     * authority, and two copies of a fact eventually disagree. This one wins.
     */
    @Synchronized
    fun stage(context: Context, unpacked: File, id: String, minHost: Int, serial: Long): Boolean {
        if (!File(unpacked, "index.html").isFile) {
            // The same completeness marker `scripts/build-ui.js` uses. An
            // archive without one is not a UI, and staging it would trade a
            // working app for a blank screen on the next launch.
            Log.e(TAG, "the unpacked bundle $id has no index.html; discarding it")
            unpacked.deleteRecursively()
            return false
        }

        val record = buildJsonObject {
            put("id", id)
            put("minHost", minHost)
            put("serial", serial)
        }
        val written = runCatching { File(unpacked, MANIFEST).writeText(record.toString()) }
        if (written.isFailure) {
            Log.e(TAG, "could not write the manifest for $id: ${written.exceptionOrNull()?.message}")
            unpacked.deleteRecursively()
            return false
        }

        val pending = File(uiDir(context), PENDING)
        pending.deleteRecursively()
        if (!unpacked.renameTo(pending)) {
            // Same directory, same filesystem — this should not happen, and if
            // it does the half-state is one `activate` already copes with.
            Log.e(TAG, "could not stage bundle $id as pending")
            unpacked.deleteRecursively()
            return false
        }
        Log.i(TAG, "bundle $id is staged")
        return true
    }

    /**
     * The id already waiting to go live, if any. Keeps the updater from
     * refetching it, and is what `MainActivity.applyStagedBundle` acts on.
     *
     * Null when [enabled] is false, which is load-bearing rather than tidy: the
     * applier reads this, and a build that reported a pending bundle it would
     * then refuse to activate would rebuild its WebView on every idle moment,
     * for ever.
     */
    @Synchronized
    fun pending(context: Context): String? =
        if (!enabled) null else readManifest(File(uiDir(context), PENDING))?.id

    // -----------------------------------------------------------------------
    // Resolution
    // -----------------------------------------------------------------------

    /**
     * Decide what to serve, and spend one of the current bundle's attempts.
     *
     * Synchronous file work on the main thread, which is deliberate: the answer
     * is needed before the WebView is created, and it is a handful of `stat`s.
     * Doing it off-thread would only mean the page loads later.
     */
    @Synchronized
    fun resolve(context: Context): Loaded {
        if (!enabled) {
            // Deliberately not [floor], which clears the probation record: this
            // build is ignoring what is on disk, not passing judgement on it.
            Log.i(TAG, "over-the-air updates are off in this build; serving the shipped bundle")
            onProbation = false
            return Loaded(SHIPPED, null).also { loaded = it }
        }
        val ui = uiDir(context)

        // Each demotion changes what `current` is, so the question has to be
        // asked again. Bounded because every demotion removes a directory:
        // current, then previous, then there is nothing left to demote.
        repeat(3) {
            val dir = File(ui, CURRENT)
            val bundle = readManifest(dir)

            if (bundle == null) {
                if (!dir.exists() && !File(ui, PREVIOUS).exists()) return floor(ui)
                Log.w(TAG, "the live bundle is unusable; falling back")
                demote(ui)
                return@repeat
            }

            val attempts = probationFor(ui, bundle.id)
            if (attempts != null && attempts <= 0) {
                Log.w(
                    TAG,
                    "bundle ${bundle.id} never reached the bridge handshake in " +
                        "$ATTEMPTS launches; rolling back"
                )
                demote(ui)
                return@repeat
            }
            if (attempts != null) {
                writeProbation(ui, bundle.id, attempts - 1)
                onProbation = true
                Log.i(TAG, "serving bundle ${bundle.id} on probation, ${attempts - 1} attempts left")
            } else {
                onProbation = false
                Log.i(TAG, "serving bundle ${bundle.id}")
            }

            return Loaded(bundle.id, dir, bundle.serial).also { loaded = it }
        }

        return floor(ui)
    }

    /** The copy inside the APK. Always present, which is the whole point. */
    private fun floor(ui: File): Loaded {
        clearProbation(ui)
        onProbation = false
        Log.i(TAG, "serving the shipped bundle")
        return Loaded(SHIPPED, null).also { loaded = it }
    }

    /**
     * `current` is bad: drop it and promote `previous` into its place.
     *
     * Moving rather than serving `previous` where it lies, so that the state on
     * disk converges. Left in place, a broken `current` would be re-judged
     * every launch for ever and the next update would be applied on top of it.
     */
    private fun demote(ui: File) {
        clearProbation(ui)
        File(ui, CURRENT).deleteRecursively()
        val previous = File(ui, PREVIOUS)
        if (previous.exists() && !previous.renameTo(File(ui, CURRENT))) {
            Log.e(TAG, "could not restore the previous bundle; falling through to the shipped one")
            previous.deleteRecursively()
        }
    }

    /**
     * The page reached `__bridge:ready`: the bundle works.
     *
     * This is the only evidence worth trusting. A bundle that renders nothing
     * still runs its scripts, and a bundle that throws on its first chunk never
     * gets here — which is exactly the case the counter exists for.
     */
    fun confirm(context: Context) {
        if (!onProbation) return
        synchronized(this) {
            if (!onProbation) return
            onProbation = false
            clearProbation(uiDir(context))
            Log.i(TAG, "bundle ${active()} confirmed")
        }
    }

    // -----------------------------------------------------------------------
    // Disk
    // -----------------------------------------------------------------------

    private fun uiDir(context: Context): File =
        File(context.filesDir, UI).apply { mkdirs() }

    private data class Manifest(val id: String, val minHost: Int, val serial: Long)

    /**
     * Read a bundle directory's identity, or null if it is not one we may serve.
     *
     * Three ways to fail, all of them meaning "do not serve this":
     *
     *  - no `index.html` — the same marker `scripts/build-ui.js` uses, because a
     *    half-copied export otherwise stages silently and shows a blank screen
     *  - no readable `bundle.json`, or no `id` in it
     *  - `minHost` above this host's [BridgeRouter.HOST_REVISION]
     *
     * The last is belt and braces: the manifest server already filters on the
     * revision the update check sent it. The server is not the only thing that
     * can be wrong, and the cost of being wrong here is a UI calling channels
     * this binary has never heard of — which hangs, silently, for ever.
     */
    private fun readManifest(dir: File): Manifest? {
        if (!File(dir, "index.html").isFile) return null
        val text = runCatching { File(dir, MANIFEST).readText() }.getOrNull() ?: return null
        val parsed = runCatching { json.parseToJsonElement(text).jsonObject }.getOrNull() ?: return null

        val id = parsed["id"]?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() } ?: return null
        val minHost = parsed["minHost"]?.jsonPrimitive?.intOrNull ?: 0
        if (minHost > BridgeRouter.HOST_REVISION) {
            Log.w(
                TAG,
                "bundle $id needs host revision $minHost and this host is " +
                    "${BridgeRouter.HOST_REVISION}; refusing it"
            )
            return null
        }
        // Absent means a bundle staged by hand — `docs/ota-bundles.md` — which
        // is worth keeping working. Serial 0 makes it replaceable by any real
        // release, which is the right answer for something pushed over adb.
        val serial = parsed["serial"]?.jsonPrimitive?.longOrNull ?: 0L
        return Manifest(id, minHost, serial)
    }

    /** Attempts remaining for [id], or null if it is not on probation. */
    private fun probationFor(ui: File, id: String): Int? {
        val text = runCatching { File(ui, PROBATION).readText() }.getOrNull() ?: return null
        val parsed = runCatching { json.parseToJsonElement(text).jsonObject }.getOrNull() ?: return null
        // A record naming a different bundle is stale — the bundle it judged is
        // gone. Treating it as ours would spend a confirmed bundle's attempts.
        if (parsed["bundle"]?.jsonPrimitive?.contentOrNull != id) return null
        return parsed["attempts"]?.jsonPrimitive?.intOrNull
    }

    /**
     * Write the record, atomically.
     *
     * A torn file reads as absent, which reads as "confirmed" — the one wrong
     * answer that matters, because it would let a fatal bundle retry for ever.
     * Writing beside it and renaming means the file is either the old record or
     * the new one.
     */
    private fun writeProbation(ui: File, id: String, attempts: Int) {
        val record = buildJsonObject {
            put("bundle", id)
            put("attempts", attempts)
        }
        val tmp = File(ui, "$PROBATION.tmp")
        runCatching {
            tmp.writeText(record.toString())
            if (!tmp.renameTo(File(ui, PROBATION))) throw java.io.IOException("rename failed")
        }.onFailure {
            tmp.delete()
            Log.e(TAG, "could not record probation for $id: ${it.message}")
        }
    }

    private fun clearProbation(ui: File) {
        File(ui, PROBATION).delete()
    }
}
