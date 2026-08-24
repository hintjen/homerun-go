package app.gethomerun.mobile

import android.content.Context
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive

/**
 * Where this process keeps "who is signed in, and which backend".
 *
 * Both answers already lived in `homerun-host` prefs, written by the bridge
 * when the page hands over credentials. What they did not have is a way for
 * anything *outside* the bridge to read them — and the notification's Stop
 * action is outside the bridge by definition: it is the control that exists
 * precisely when no page is in front of the user.
 *
 * The prefs name and both keys are duplicated in `BridgeRouter` and
 * `DeviceRegistry`, which predate this file. That is a drift risk and worth
 * collapsing — the string "homerun-host" appearing in three files is one
 * rename away from a component quietly reading an empty store. Not done here
 * because it touches two large files for no behaviour change; this object is
 * the place for it when someone does.
 *
 * Nothing here caches. A token read once and held would outlive the logout
 * that revoked it, and these are read on paths that run at most once per
 * player action.
 */
object HostSession {

    private val json = Json { ignoreUnknownKeys = true }

    /**
     * The backend this build talks to, as the page last set it.
     *
     * Null when the page has never overridden it, which is the normal case:
     * the caller should fall back to `BuildConfig.API_URL`. Logout removes the
     * override deliberately — see the `logout` handler in `BridgeRouter` — so
     * a staging override does not outlive the session that chose it.
     */
    fun apiUrl(context: Context): String? =
        context.applicationContext
            .getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(KEY_API_URL, null)
            ?.takeIf { it.isNotBlank() }

    /**
     * The signed-in user's access token, or "" when nobody is signed in.
     *
     * The **user** token, not the device token: the two are not
     * interchangeable and the difference is a rule, not an implementation
     * detail. Reporting what a device is doing is signed with the device
     * token; changing what a server is *meant* to be doing is a settings
     * change and is signed as the person who asked for it.
     */
    fun userToken(context: Context): String = runCatching {
        val prefs = context.applicationContext
            .getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val stored = SecretStore.read(prefs, KEY_CREDENTIALS) ?: return@runCatching ""
        (json.parseToJsonElement(stored) as? JsonObject)
            ?.get("access_token")?.jsonPrimitive?.contentOrNull.orEmpty()
    }.getOrDefault("")

    private const val PREFS = "homerun-host"
    private const val KEY_API_URL = "api-url"
    private const val KEY_CREDENTIALS = "credentials"
}
