package app.gethomerun.mobile

import android.content.Context
import android.content.SharedPreferences
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive

/**
 * The signed-in user's credentials, for the few things that act *as them*.
 *
 * # Why this is not just a field on the bridge
 *
 * It used to be. The bridge holds the token because login flows through it,
 * and everything that needed one — registration, the tunnel poll — was called
 * from a bridge handler that already had it in scope.
 *
 * Ops sync broke that. A `/op` typed into the console has to be mirrored into
 * the server's settings on the API, and the API judges that change against
 * **the person who asked for it**: a member's settings PATCH is stripped
 * server-side, matching their inability to change ops in the UI. So the
 * request must be signed by a user, not by the device — and the code that
 * decides to send it is a console watcher, not a bridge handler.
 *
 * # The rule this object exists to keep
 *
 * The token is read here and nowhere else, and it must never reach a server
 * process. [ServerConfig.extra] is forwarded into the child's environment;
 * anything in this file that ended up there would hand a Minecraft server —
 * and any plugin in it — the user's account.
 */
object Session {

    private lateinit var prefs: SharedPreferences

    fun init(context: Context) {
        prefs = context.applicationContext
            .getSharedPreferences(PREFS, Context.MODE_PRIVATE)
    }

    /**
     * The user's access token, or null if nobody is signed in.
     *
     * Read fresh every time rather than cached: it is rewritten on refresh,
     * and a cached copy would go stale mid-session in a way that surfaces as
     * an unexplained 401 on whichever call happened to be next.
     */
    fun userToken(): String? {
        if (!::prefs.isInitialized) return null
        return runCatching {
            val stored = SecretStore.read(prefs, KEY_CREDENTIALS) ?: return null
            (json.parseToJsonElement(stored) as? JsonObject)
                ?.get("access_token")?.jsonPrimitive?.contentOrNull
                ?.takeIf { it.isNotBlank() }
        }.getOrNull()
    }

    private val json = Json { ignoreUnknownKeys = true }

    /** The bridge's own preferences file — the same one it writes at login. */
    private const val PREFS = "homerun-host"
    private const val KEY_CREDENTIALS = "credentials"
}
