package app.gethomerun.mobile

import android.app.Activity
import android.app.Application
import android.os.Bundle
import java.lang.ref.WeakReference

/**
 * Whichever activity is on screen, or null when none is.
 *
 * One caller needs this, for one reason. Play raises a confirmation before it
 * downloads a feature module over a metered connection, and that dialog has to
 * be started *on an Activity*. [JavaRuntime.fetchModule] runs on an IO thread
 * deep inside a launch that a bridge call began, so it has no activity of its
 * own and no result to return to — and without one, every Java server started
 * on a mobile connection would refuse instead of asking.
 *
 * The reference is weak so an activity that finishes while this still points
 * at it is collected rather than pinned by a process-scoped singleton.
 *
 * **Null is an ordinary answer, not a failure.** A server can be started and
 * run entirely with the app in the background — that is what the foreground
 * service exists for — and a dialog cannot be raised then. The caller falls
 * back to a sentence the player can act on.
 */
object ForegroundActivity {

    private var current: WeakReference<Activity>? = null

    /** Registered once, from [HomerunApplication]. */
    fun track(application: Application) {
        application.registerActivityLifecycleCallbacks(
            object : Application.ActivityLifecycleCallbacks {
                override fun onActivityResumed(activity: Activity) {
                    current = WeakReference(activity)
                }

                /**
                 * Cleared on the way out, but only if this is still the one
                 * being tracked. A pause that arrives after some other
                 * activity has already resumed would otherwise drop the
                 * activity that is now actually on screen.
                 */
                override fun onActivityPaused(activity: Activity) {
                    if (current?.get() === activity) current = null
                }

                override fun onActivityCreated(activity: Activity, state: Bundle?) = Unit
                override fun onActivityStarted(activity: Activity) = Unit
                override fun onActivityStopped(activity: Activity) = Unit
                override fun onActivitySaveInstanceState(activity: Activity, out: Bundle) = Unit
                override fun onActivityDestroyed(activity: Activity) = Unit
            },
        )
    }

    /**
     * Something to show a dialog on, or null.
     *
     * A finishing or destroyed activity answers null: starting a dialog on one
     * throws, and the caller wants its fallback rather than an exception.
     */
    fun get(): Activity? = current?.get()?.takeUnless { it.isFinishing || it.isDestroyed }
}
