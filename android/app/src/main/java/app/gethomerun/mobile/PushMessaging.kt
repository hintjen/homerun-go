package app.gethomerun.mobile

import android.util.Log
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine

/**
 * The host's half of remote push: the FCM token, and nothing else.
 *
 * The split (bridge contract, `remotePush`): this host owns what must be
 * native — the OS permission and the token Firebase mints. Registering that
 * token with the API is the *UI's* job, over the user's own JWT
 * (`POST /api/push/devices/`), exactly like social sign-in. That is why
 * nothing in this file talks to the network beyond Firebase itself, and why
 * no user identity appears anywhere in it.
 *
 * Delivery is mostly not our code either. A message sent while the app is
 * backgrounded is drawn by the system tray from its `notification` block —
 * the process may not even be running — using the channel id the API sender
 * puts in the message (`"homerun"`, the same channel the local
 * `push-notification` bridge channel posts on, so the user has one mute
 * switch). The channel itself is created at process start in
 * [HomerunApplication]: a notification naming a channel that does not exist
 * is silently dropped, and the tray renders background pushes long before
 * any code in this app has run.
 */
object PushMessaging {

    /**
     * The router of the current activity, for emitting `push:token-changed`.
     * Set in `MainActivity.onCreate`, cleared in `onDestroy` — same lifetime
     * as every other router reference, so a recreated activity does not leave
     * a dead router receiving tokens.
     */
    @Volatile
    var router: BridgeRouter? = null

    /**
     * The token FCM most recently minted, so `push:get-token` can answer
     * without a Firebase round trip once one has arrived. Null until then —
     * which the contract calls a state, not an error: an emulator without
     * Play services stays null forever and nothing may break.
     */
    @Volatile
    var lastToken: String? = null
        private set

    fun onNewToken(token: String) {
        lastToken = token
        // The UI re-upserts to the API on every firing; a rotation the UI
        // never hears about is a phone that silently stops receiving.
        router?.emit("push:token-changed", listOf(kotlinx.serialization.json.JsonPrimitive(token)))
    }

    /**
     * Ask Firebase for the current token. Resolves null rather than throwing
     * when there is none to be had (no Play services, no permission for the
     * IID backend, aeroplane mode) — every caller treats null as "not yet".
     */
    suspend fun currentToken(): String? {
        lastToken?.let { return it }
        return runCatching {
            suspendCancellableCoroutine<String?> { continuation ->
                FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
                    if (task.isSuccessful) {
                        continuation.resume(task.result)
                    } else {
                        Log.i(TAG, "no FCM token available: ${task.exception?.message}")
                        continuation.resume(null)
                    }
                }
            }
        }.getOrNull()?.also { lastToken = it }
    }

    private const val TAG = "HomerunPush"
}

/**
 * FCM's entry points into the process. Declared in the manifest; the system
 * instantiates it, so it holds no state of its own — everything goes through
 * [PushMessaging].
 */
class PushMessagingService : FirebaseMessagingService() {

    override fun onNewToken(token: String) {
        PushMessaging.onNewToken(token)
    }

    /**
     * Only reached while the app is foregrounded (background messages with a
     * `notification` block never get here — the tray draws them). And a
     * foregrounded app **suppresses the banner, deliberately**: the app being
     * open is the notification surface — the bell and the server card are
     * already showing the event — and a system banner on top of a page that
     * says the same thing is noise. "Foreground" here means our activity is
     * actually visible; a minimized app takes the tray path and the banner
     * shows, which is the case the push exists for.
     *
     * The receipt log is load-bearing: with no banner, this line is the only
     * evidence a foreground delivery happened at all, and "delivered but
     * suppressed" must stay distinguishable from "never delivered".
     */
    override fun onMessageReceived(message: RemoteMessage) {
        Log.i(
            "HomerunPush",
            "message received in foreground, banner suppressed: " +
                "notification=${message.notification != null} data=${message.data.keys}",
        )
    }
}
