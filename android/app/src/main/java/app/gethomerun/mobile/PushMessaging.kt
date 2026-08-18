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
     * `notification` block never get here — the tray draws them). The player
     * is already looking at the screen, and the page in front of them is the
     * surface that should react; still post the tray notification, because a
     * foregrounded app on another screen is exactly when "your server
     * crashed" must not be droppable.
     */
    override fun onMessageReceived(message: RemoteMessage) {
        val title = message.notification?.title
        val body = message.notification?.body ?: return
        BridgeRouter.postNotification(
            applicationContext,
            title = title ?: getString(R.string.app_name),
            body = body,
        )
    }
}
