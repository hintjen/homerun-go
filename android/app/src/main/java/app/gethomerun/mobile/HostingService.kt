package app.gethomerun.mobile

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/**
 * What keeps a server running once the app is no longer in front of the user.
 *
 * # Why a service at all
 *
 * The Minecraft server is a child process of this app's process, spawned by the
 * supervisor in `homerun-pumpkin-ffi`. Android does not know or care that it
 * exists: it accounts for the *app* process, and once that process has no
 * visible activity it becomes a cached process — first in line for the
 * low-memory killer, on a device that is simultaneously being asked to run a
 * JVM with a gigabyte of heap. Killing it takes the JVM and the supervisor with
 * it, and what a player sees is a server that vanished when they answered a
 * text message.
 *
 * A foreground service is the only mechanism Android offers to say "this
 * process is doing something the user asked for and can see". It raises the
 * process to foreground importance for as long as it runs. The notification is
 * not a courtesy — it is the consideration the platform charges for that
 * priority, and it is also the only control a player has while the app is not
 * open.
 *
 * # This service decides nothing
 *
 * [ServerHost] decides when hosting is happening and starts and stops this
 * accordingly; this class renders that state into a notification and holds the
 * wake lock. The split matters because "is this device still busy" has a
 * subtle answer — a stopped server with a backup still uploading is busy — and
 * that answer should exist in exactly one place. See [ServerHost.Hosting].
 *
 * # A backup is hosting too
 *
 * The world upload runs for minutes *after* the server has stopped, and losing
 * it is worse than losing a running server: the run that just finished is the
 * one not yet in the repository, so a killed backup is a lost session. The
 * service therefore outlives the server it started for, which is why
 * [ServerHost] and not the state machine owns the stop.
 */
class HostingService : Service(), ServerHost.Listener {

    /**
     * Its own scope, not [ServerHost]'s: the only thing launched here is the
     * notification's Stop, and a stop that outlived the service would be
     * holding a notification nobody can see any more.
     *
     * Its own scope needs its own handler too ([ServerHost.keepAlive]) — a
     * `SupervisorJob` does not stop a throw reaching the default handler. What
     * that keeps alive is this service and therefore the whole session: the
     * foreground priority, the wake lock, and the JVM and backup underneath
     * them. [ServerHost.stop] calls into the core and the backend and both can
     * throw, and this is the one path a player takes while the app is not in
     * front of them — a failed Stop must leave them a server they can try to
     * stop again, not a process that took the world down with it mid-save.
     */
    private val scope = CoroutineScope(
        SupervisorJob() + Dispatchers.Default + ServerHost.keepAlive(TAG, "the notification's Stop"),
    )

    private var wakeLock: PowerManager.WakeLock? = null

    /**
     * Whether [startForeground] has run. Calling [NotificationManager.notify]
     * for this id before it would post a notification the service does not own,
     * which then survives the service and cannot be dismissed.
     */
    private var foreground = false

    override fun onCreate() {
        super.onCreate()
        createChannel()
        acquireWakeLock()
        ServerHost.addListener(this)
    }

    /**
     * Delivered on every `startForegroundService`, which [ServerHost] calls
     * whenever hosting begins and again on any change while it is running — so
     * this must be idempotent, and is.
     *
     * `START_NOT_STICKY` on purpose. If this process is killed the server died
     * with it, because the JVM was its child; a service Android restarts by
     * itself would come up showing a notification for a server that is not
     * running and holding a wake lock for nothing.
     */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Before anything that could throw or return early: Android gives a
        // service started with startForegroundService five seconds to call
        // this, and kills the app with a ForegroundServiceDidNotStartInTime
        // crash if it does not.
        goForeground()

        if (intent?.action == ACTION_STOP) stopHosting()

        return START_NOT_STICKY
    }

    override fun onDestroy() {
        ServerHost.removeListener(this)
        releaseWakeLock()
        scope.cancel()
        super.onDestroy()
    }

    /** Started, never bound — the bridge reaches the backend through [ServerHost]. */
    override fun onBind(intent: Intent?): IBinder? = null

    /**
     * The user swiped the app out of Recents while a server was running.
     *
     * Nothing happens, deliberately. `stopWithTask` is false and this is the
     * one case where Android's default is what we want: a player who swipes
     * the app away has not asked their friends to be disconnected, and the
     * notification's Stop action is right there when they do. Hosting outliving
     * the task is the same bargain a music player makes.
     */
    override fun onTaskRemoved(rootIntent: Intent?) {
        Log.i(TAG, "task removed; hosting continues (${ServerHost.hosting().state})")
        super.onTaskRemoved(rootIntent)
    }

    // -----------------------------------------------------------------------
    // Reacting to the server
    // -----------------------------------------------------------------------

    override fun onStateChanged(serverId: String, state: ServerState, backupInProgress: Boolean) {
        refresh()
    }

    override fun onPlayersChanged(serverId: String) {
        refresh()
    }

    /**
     * Repost the notification with current facts.
     *
     * [NotificationManager.notify] on the same id, not another
     * [startForeground] — the service is already in the foreground and
     * re-entering it on every player join would be a no-op at best.
     */
    private fun refresh() {
        if (!foreground) return
        runCatching { manager().notify(NOTIFICATION_ID, build(ServerHost.hosting())) }
            .onFailure { Log.w(TAG, "could not refresh the hosting notification: ${it.message}") }
    }

    /**
     * The notification's Stop, which must be the *same* stop the UI's button
     * performs — including the core's graceful-or-not verdict, because getting
     * that wrong here would terminate a JVM that had a world to save.
     * [ServerHost.stop] is that one implementation.
     *
     * And the same *record* of it. Stopping the process is only half of what
     * the app's Stop button does: the page PATCHes `target_state` afterwards,
     * which is what tells the API the player wants this server off. The
     * notification has no page — it is the control that exists for when the
     * app is not in front of anyone — so that half was simply missing, and
     * `useNativeServerReconcile` did what it is supposed to do with a server
     * that is meant to be running and is not: it started it again. Stop
     * worked, and then the card said "Starting…".
     */
    private fun stopHosting() {
        val hosting = ServerHost.hosting()
        val serverId = hosting.serverId
        if (serverId == null) {
            Log.w(TAG, "Stop was tapped with nothing hosting; standing down")
            ServerHost.syncHosting()
            return
        }
        Log.i(TAG, "Stop tapped in the notification for $serverId")
        scope.launch {
            val refusal = ServerHost.stop(serverId)
            if (refusal != null) {
                // Nothing was stopped, so nothing should be recorded — a
                // `target_state` of stopped against a server that is still up
                // would have the reconcile stop it out from under the player.
                Log.w(TAG, "notification stop refused: $refusal")
                return@launch
            }
            markStoppedRemotely(serverId, hosting.name)
        }
    }

    /**
     * Tell the API the player wants this server off, the way the page would.
     *
     * Deliberately after [ServerHost.stop] has returned rather than before it:
     * a stopping server is still this device's, and the API reading `stopped`
     * while the world is still saving is what the ordering comment on
     * `ServerHost.stop` warns about from the other direction.
     */
    private suspend fun markStoppedRemotely(serverId: String, serverName: String?) {
        val apiUrl = HostSession.apiUrl(this) ?: BuildConfig.API_URL
        val token = HostSession.userToken(this)
        if (token.isBlank()) {
            // Signed out with a server running. The reconcile loop needs a
            // session too, so nothing is going to restart it either.
            Log.i(TAG, "no session; not recording the stop of $serverId")
            return
        }
        if (!HomerunApi.markStopped(apiUrl, serverId, serverName, token)) {
            Log.w(TAG, "the stop of $serverId was not recorded; reconcile may restart it")
        }
    }

    // -----------------------------------------------------------------------
    // The notification
    // -----------------------------------------------------------------------

    private fun manager(): NotificationManager =
        getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    /**
     * `IMPORTANCE_LOW`: visible and persistent, but silent. A server that has
     * been running for six hours must not make a sound every time somebody
     * joins, and the notification is refreshed on every player change.
     */
    private fun createChannel() {
        manager().createNotificationChannel(
            NotificationChannel(
                CHANNEL,
                getString(R.string.hosting_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = getString(R.string.hosting_channel_description)
                setShowBadge(false)
            },
        )
    }

    private fun goForeground() {
        val notification = build(ServerHost.hosting())
        // From API 34 the type passed here must match one declared in the
        // manifest. `specialUse` is the honest declaration for hosting a game
        // server: it is not a media session, not a transfer, and not a
        // companion device. `dataSync` would fit the backup half and is
        // deliberately not used — Android 15 caps a dataSync service at six
        // hours a day, which is a limit on how long somebody may play.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        foreground = true
    }

    private fun build(hosting: ServerHost.Hosting): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        val builder = Notification.Builder(this, CHANNEL)
            .setContentTitle(hosting.name ?: getString(R.string.app_name))
            .setContentText(text(hosting))
            // Not `applicationInfo.icon`. A small icon is drawn from its alpha
            // channel alone, and the launcher icon's background layer is an
            // opaque square — it comes out as a featureless blob. See
            // res/drawable/ic_notification.xml.
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(getColor(R.color.brand_cornflower))
            .setContentIntent(open)
            // Not dismissable while hosting: the notification is how a player
            // gets back to the app and how they stop the server, and a swipe
            // that hid it would leave a running server with no visible
            // control at all.
            .setOngoing(true)
            .setShowWhen(false)

        // Nothing to stop yet, or nothing left to stop — a Stop action during
        // the backup would suggest the upload can be cancelled, and it is the
        // one thing here worth protecting.
        if (hosting.stoppable) {
            val stop = PendingIntent.getService(
                this,
                1,
                Intent(this, HostingService::class.java).setAction(ACTION_STOP),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            builder.addAction(
                Notification.Action.Builder(null, getString(R.string.hosting_stop), stop).build(),
            )
        }

        return builder.build()
    }

    /**
     * One line, written for a player glancing at their lock screen rather than
     * for a log. The player count is the useful fact while a server is up; on
     * the way in and out, what is happening is.
     */
    private fun text(hosting: ServerHost.Hosting): String = when {
        hosting.backingUp -> getString(R.string.hosting_backing_up)
        hosting.state == ServerState.RUNNING -> when (hosting.players) {
            null -> getString(R.string.hosting_running)
            0 -> getString(R.string.hosting_running_empty)
            1 -> getString(R.string.hosting_running_one)
            else -> getString(R.string.hosting_running_many, hosting.players)
        }
        hosting.state == ServerState.STOPPING -> getString(R.string.hosting_stopping)
        // Everything else that got this service started is a launch in
        // progress, including the window before the backend has announced
        // anything — see `Hosting.starting`. A jar download is minutes long and
        // "Starting…" is the only honest thing to say during it.
        else -> getString(R.string.hosting_starting)
    }

    // -----------------------------------------------------------------------
    // Staying awake
    // -----------------------------------------------------------------------

    /**
     * A foreground service keeps the process from being *killed*; it does not
     * keep the CPU running. With the screen off the device suspends between
     * wakeups, and a suspended CPU is a server that stops ticking — clients
     * time out and the world stops saving. A partial wake lock is what a player
     * is actually asking for when they put the phone down and keep hosting.
     *
     * No timeout, which lint dislikes and is correct here: the lock's lifetime
     * is the service's, the service's lifetime is the hosting session's, and
     * both ends are explicit. A timeout would end a session mid-game.
     */
    private fun acquireWakeLock() {
        wakeLock = runCatching {
            val power = getSystemService(Context.POWER_SERVICE) as PowerManager
            power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG).apply {
                setReferenceCounted(false)
                acquire()
            }
        }.onFailure {
            // Worth a line: hosting still works, but a server that goes
            // unresponsive whenever the screen is off looks like a network bug
            // and is not one.
            Log.w(TAG, "no wake lock — hosting may stall with the screen off: ${it.message}")
        }.getOrNull()
    }

    private fun releaseWakeLock() {
        runCatching { wakeLock?.takeIf { it.isHeld }?.release() }
            .onFailure { Log.w(TAG, "could not release the wake lock: ${it.message}") }
        wakeLock = null
    }

    companion object {
        private const val TAG = "HomerunHosting"

        /** Separate from the bridge's `homerun` channel, which is for alerts. */
        private const val CHANNEL = "homerun-hosting"
        private const val NOTIFICATION_ID = 1
        private const val WAKE_LOCK_TAG = "homerun:hosting"
        private const val ACTION_STOP = "app.gethomerun.mobile.STOP_HOSTING"

        /**
         * Bring the service up, or refresh it if it is already up. Returns
         * whether Android accepted the start.
         *
         * `startForegroundService` from the background is blocked on API 26+,
         * and mostly that is not a problem this hits: hosting begins because a
         * user tapped Start in the app, so an activity is in front of us. It is
         * caught rather than allowed to crash because the *refresh* calls
         * arrive later, from a state change that could in principle land after
         * the app has gone away.
         *
         * **The return value is load-bearing and must not be dropped.**
         * [ServerHost.syncHosting] latches "the service is up" so it can skip
         * redundant starts, and a swallowed failure would latch a service that
         * never came up: a server still running, with an untimed wake lock, no
         * foreground protection and no notification — the exact state the
         * `specialUse` declaration promises cannot happen, and the one most
         * likely to end with the low-memory killer taking the JVM mid-save.
         */
        fun start(context: Context): Boolean {
            val intent = Intent(context, HostingService::class.java)
            return try {
                context.startForegroundService(intent)
                true
            } catch (err: Exception) {
                // Named rather than lumped in with the rest, because it means
                // "you asked from the background" rather than "something is
                // broken", and it is the failure a start racing the app's own
                // exit actually produces. API 31+; the version guard is what
                // keeps the class reference off older devices' verify path.
                val refused = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
                    err is android.app.ForegroundServiceStartNotAllowedException
                if (refused) {
                    Log.e(TAG, "Android refused a foreground-service start from the background", err)
                } else {
                    Log.e(TAG, "could not start the hosting service: ${err.message}", err)
                }
                false
            }
        }

        fun stop(context: Context) {
            runCatching { context.stopService(Intent(context, HostingService::class.java)) }
                .onFailure { Log.w(TAG, "could not stop the hosting service: ${it.message}") }
        }
    }
}
