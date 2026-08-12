package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import java.io.File
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
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

            return Loaded(bundle.id, dir).also { loaded = it }
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

    private data class Manifest(val id: String, val minHost: Int)

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
        return Manifest(id, minHost)
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
