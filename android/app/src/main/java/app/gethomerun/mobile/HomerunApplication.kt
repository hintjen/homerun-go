package app.gethomerun.mobile

import android.app.Application
import android.os.Build
import android.util.Log
import android.webkit.WebView

class HomerunApplication : Application() {
    override fun onCreate() {
        super.onCreate()

        // First, so both cover the rest of this method as well. `AppErrors`
        // goes before the handler because the handler stashes through it.
        AppErrors.init(this)
        logCrashesBeforeDying()

        // Process-scoped, because a running server must survive the activity
        // and the WebView being torn down and rebuilt.
        ServerHost.init(this)

        // The alerts channel, before anything can post on it — including the
        // system tray drawing a background push, which happens with no other
        // code from this app running. A notification naming a channel that
        // does not exist is silently dropped.
        BridgeRouter.ensureNotificationChannel(this)

        // Reads the same preferences the bridge writes at login, and must be
        // ready before anything that acts as the user.
        Session.init(this)

        // Listens for the life of the process, for the same reason the backend
        // does: a page reload must not silence reporting for a server that is
        // still running.
        Reporting.init()

        // The heartbeat starts with the process, not with a login or a server.
        // The API marks a device unhealthy 60 s after its last report, and a
        // relaunch already holds the token issued last time.
        DeviceRegistry.init(this) { ServerHost.backend.runningServerIds }

        // Process-scoped for the same reason the backend is: the device link
        // takes up to a minute to provision and must outlive any page. The
        // scope is ServerHost's — cancelled never, because the thing that ends
        // it is the process ending.
        DeviceWebsocket.init(this, ServerHost.scope)

        // Last, because it needs a credential to sign with and a place to
        // send to. What it sends was written before any of the above ran —
        // during the previous launch, by a process that did not survive to
        // report it itself.
        AppErrors.drain()

        // Debug builds are inspectable from the host machine at
        // chrome://inspect — the only practical way to debug the shared UI
        // running inside the app.
        if (BuildConfig.DEBUG) {
            WebView.setWebContentsDebuggingEnabled(true)
        }

        // Every process that touches a WebView needs its own data directory.
        // Hosting will eventually run in its own process, and the second
        // WebView to start would otherwise abort the app on a lock conflict
        // whose stack trace says nothing about the real cause.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            val process = Application.getProcessName()
            if (process != packageName) {
                WebView.setDataDirectorySuffix(process.substringAfter(':', "alt"))
            }
        }
    }

    /**
     * One line of context before the process goes, and then the platform's own
     * handling, untouched.
     *
     * Nothing in this app reports crashes natively. The shared bundle carries
     * `@sentry/electron`, which is a *renderer* integration looking for an
     * Electron main process that does not exist here — it sees the page's
     * errors and cannot see a Kotlin stack at all (docs/android-host.md). So
     * what a crash leaves behind is the logcat tombstone, and the one thing a
     * tombstone cannot say is what this device was doing at the time: a crash
     * during a world upload is a lost session and looks exactly like any other
     * crash. [ServerHost.hostingSummary] is deliberately the lock-free, JNI-free
     * reading of that — this runs on the thread that is dying.
     *
     * **It always delegates.** Replacing the platform's handler instead of
     * wrapping it would suppress the tombstone, the crash dialog, and Play's
     * vitals — every crash would become invisible to everyone but us. Android
     * always installs one (`RuntimeInit`'s `KillApplicationHandler`), so the
     * null branch is theory. `runCatching` for the same reason: a handler that
     * throws is a handler that never reaches the line below it.
     *
     * It now also stashes the crash for [AppErrors] to send next launch, which
     * is the half a tombstone could never provide: the stack, off the device,
     * grouped with every other sighting of the same bug.
     */
    private fun logCrashesBeforeDying() {
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { thread, err ->
            runCatching {
                Log.e(TAG, "fatal on ${thread.name}; this device was ${ServerHost.hostingSummary()}", err)
            }
            // Stashed, not sent. This thread is about to be killed by the
            // handler below, so a coroutine would never resume and a request
            // would never complete — the report that matters most would be
            // the one guaranteed to be lost. It goes to disk here and leaves
            // on the next launch. Wrapped for the same reason as the log
            // above: a handler that throws never reaches the line after it.
            runCatching { AppErrors.stash(err, location = thread.name) }
            previous?.uncaughtException(thread, err)
        }
    }

    private companion object {
        const val TAG = "HomerunHost"
    }
}
