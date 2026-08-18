package app.gethomerun.mobile

import android.app.Activity
import android.app.Application
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CompletableDeferred
import java.util.concurrent.atomic.AtomicInteger

/**
 * The system share sheet, for `share-content`.
 *
 * The UI hands over a title, a sentence and a link and asks the OS to place
 * them. That is worth doing rather than copying to the clipboard because the
 * platform sheet ranks targets by who this person actually shares with, and
 * offers individual conversations — Discord channels among them — as one-tap
 * targets that drop the message straight into the right compose box.
 *
 * # Knowing whether they went through with it
 *
 * The contract says `completed: false` means the user dismissed the sheet, and
 * Android makes that the awkward half. A chooser does not report a result:
 * `startActivityForResult` on it comes back `RESULT_CANCELED` whether the user
 * shared or not, because the app they picked never sets a result.
 *
 * So this reads two signals and takes whichever arrives first:
 *
 *  - **A target was picked.** `Intent.createChooser` accepts an `IntentSender`
 *    it fires when the user chooses, which is the only positive signal the
 *    platform offers.
 *  - **We are back on screen.** The sheet is its own activity, so ours pauses
 *    while it is up. Being resumed with no pick having been reported means the
 *    sheet was dismissed.
 *
 * The pick fires before the target app opens, so it always wins the race
 * against the resume that follows the user coming back.
 *
 * # It must always answer
 *
 * An invoke that never resolves leaves a UI promise pending for ever — the
 * worst failure in this protocol, and a frozen Share button with nothing in the
 * log. Every path here completes the deferred, including the ones where nothing
 * can be launched at all. The only case that does not is the user never
 * returning to the app, and by then the page that asked is gone and the router
 * has already failed its pending calls (PROTOCOL.md §4.3).
 */
object Sharing {

    /**
     * Present the sheet and report whether a target was chosen.
     *
     * Suspends until the user picks or dismisses. There is deliberately no
     * timeout: reading a share sheet is not something to put a clock on, and a
     * dismissal is detected directly rather than guessed at.
     */
    suspend fun share(
        context: Context,
        title: String?,
        text: String?,
        url: String?,
    ): Boolean {
        // The OS joins the sentence and the link itself when they arrive as one
        // body, which is why the UI sends them apart: `text` is the invite
        // without the link, so putting them together here sends it once rather
        // than twice.
        val body = listOfNotNull(text?.ifBlank { null }, url?.ifBlank { null }).joinToString(" ")
        if (body.isBlank() && title.isNullOrBlank()) {
            Log.w(TAG, "nothing to share")
            return false
        }

        val send = Intent(Intent.ACTION_SEND).apply {
            type = "text/plain"
            putExtra(Intent.EXTRA_TEXT, body.ifBlank { title.orEmpty() })
            // Mail clients and a few others use this as a subject line; the
            // sheet itself shows it as the preview title.
            title?.takeIf { it.isNotBlank() }?.let { putExtra(Intent.EXTRA_SUBJECT, it) }
        }

        val settled = CompletableDeferred<Boolean>()
        val application = context.applicationContext as? Application

        val action = "$CHOSEN_ACTION.${sequence.incrementAndGet()}"
        val chosen = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                settled.complete(true)
            }
        }
        // Exported because the sender is the system, not this app. The action
        // carries a per-call suffix so two shares in flight cannot answer each
        // other's deferred.
        ContextCompat.registerReceiver(
            context,
            chosen,
            IntentFilter(action),
            ContextCompat.RECEIVER_EXPORTED,
        )

        // Mutable so the system can attach EXTRA_CHOSEN_COMPONENT on the way
        // through. An immutable PendingIntent is silently never fired here,
        // which reads as every share being dismissed.
        val mutable =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
        val callback = PendingIntent.getBroadcast(
            context,
            0,
            Intent(action).setPackage(context.packageName),
            PendingIntent.FLAG_UPDATE_CURRENT or mutable,
        )

        val resumed = object : Application.ActivityLifecycleCallbacks {
            /**
             * Ours coming back with no pick reported is a dismissal.
             *
             * Guarded on the sheet having actually taken the screen: the
             * chooser is started from here, and on some devices the launching
             * activity reports a resume of its own before it pauses. Without
             * [left] that resume would settle the share as dismissed the
             * instant it opened.
             */
            var left = false

            override fun onActivityPaused(activity: Activity) {
                left = true
            }

            override fun onActivityResumed(activity: Activity) {
                if (left) settled.complete(false)
            }

            override fun onActivityCreated(activity: Activity, state: Bundle?) = Unit
            override fun onActivityStarted(activity: Activity) = Unit
            override fun onActivityStopped(activity: Activity) = Unit
            override fun onActivitySaveInstanceState(activity: Activity, out: Bundle) = Unit
            override fun onActivityDestroyed(activity: Activity) = Unit
        }
        application?.registerActivityLifecycleCallbacks(resumed)

        try {
            val chooser = Intent.createChooser(send, null, callback.intentSender)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(chooser)
        } catch (err: Exception) {
            // No sheet to present. Answering false is what keeps the UI's own
            // fallback honest rather than leaving the button dead.
            Log.w(TAG, "could not present the share sheet: ${err.message}")
            settled.complete(false)
        }

        return try {
            settled.await()
        } finally {
            application?.unregisterActivityLifecycleCallbacks(resumed)
            runCatching { context.unregisterReceiver(chosen) }
        }
    }

    /** Distinguishes concurrent shares. Overflow is harmless; it only has to differ. */
    private val sequence = AtomicInteger(0)

    private const val CHOSEN_ACTION = "app.gethomerun.mobile.SHARE_CHOSEN"
    private const val TAG = "HomerunShare"
}
