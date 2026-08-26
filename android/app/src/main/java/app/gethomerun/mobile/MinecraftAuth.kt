package app.gethomerun.mobile

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.util.Log
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * Signing in to a Microsoft account, so a phone can know which Minecraft player
 * it belongs to.
 *
 * # Why the phone needs to know
 *
 * Minigame stats are keyed on a Minecraft uuid, and every read of them takes
 * one as input. A phone had no way to obtain one, so its Minigames Hub was
 * permanently empty — not broken, just structurally unable to show anybody
 * their own numbers. Most people are covered without any of this, because the
 * API can report an account they linked from the desktop app; this is for
 * somebody whose only Homerun Go device is the phone in their hand.
 *
 * # Device code, and why not a redirect
 *
 * The desktop signs in with the public Xbox client id, whose only registered
 * redirect is a hosted Microsoft page it can watch a `BrowserWindow` navigate
 * to. A phone cannot watch that: intercepting a redirect to a domain we do not
 * own needs an App Link we cannot verify, and the alternative — an embedded
 * WebView — is the thing Microsoft asks people not to do and takes the user's
 * existing session away from them.
 *
 * So this uses the **device code** flow instead: ask Microsoft for a short
 * code, send the user to their real browser with it already filled in, and poll
 * until they approve. It is a standard OAuth flow meant for exactly this, and
 * it needs no app registration, no redirect URI, and no Minecraft API approval.
 *
 * If we ever get an app registration Microsoft has approved for the
 * Minecraft API, the redirect flow becomes one tap with no code to read, and
 * swapping to it means calling `authorize_url`/`redeem_request` instead of the
 * two device-code calls. Everything after the first token is identical and
 * already written — see `minecraft::account`.
 *
 * # Where the decisions are
 *
 * Not here. Every request body, every response shape and every error message is
 * `homerun_core::minecraft::account`, because the chain is five calls deep with
 * a documented trap at nearly every one, and iOS has to make the same calls.
 * This file opens sockets, sleeps between polls, and writes to [SecretStore].
 */
class MinecraftAuth(private val context: Context) {

    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
    private val json = Json { ignoreUnknownKeys = true }

    /** The signed-in session, or null. Tokens included — never leaves this class. */
    private fun stored(): JsonObject? =
        SecretStore.read(prefs, KEY_SESSION)
            ?.let { runCatching { json.parseToJsonElement(it).jsonObject }.getOrNull() }

    private fun store(session: JsonObject?) {
        SecretStore.write(prefs, KEY_SESSION, session?.toString())
    }

    /**
     * The current account, refreshed if its token has aged out, or null.
     *
     * Null is an ordinary answer — nobody has signed in — and so is null after
     * a refresh that failed, which is a session that cannot be recovered
     * without the user. The caller reports both the same way: signed out.
     */
    suspend fun profile(): JsonObject? = withContext(Dispatchers.IO) {
        val session = stored() ?: return@withContext null

        val expiresAt = session["expiresAt"]?.jsonPrimitive?.longOrNull ?: 0L
        if (!Core.accountNeedsRefresh(expiresAt, System.currentTimeMillis())) {
            return@withContext session
        }

        val refreshToken = session["refreshToken"]?.jsonPrimitive?.contentOrNull
        if (refreshToken.isNullOrEmpty()) {
            Log.w(TAG, "stored session has no refresh token — signing out")
            store(null)
            return@withContext null
        }

        try {
            // Through the core first: this body is Microsoft's own spelling,
            // unlike a poll outcome, which has already been normalised.
            val refreshed = Core.accountMsaTokensFrom(
                exchange(Core.accountRefreshRequest(refreshToken))
            )
            val session = buildSession(refreshed)
            store(session)
            session
        } catch (err: Exception) {
            // A refresh can fail because the user revoked access, changed their
            // password, or is simply offline. Only the last is temporary, and
            // this cannot tell them apart — so keep the session and report
            // signed out for now rather than deleting a recoverable login.
            Log.w(TAG, "could not refresh the Minecraft session: ${err.message}")
            null
        }
    }

    /** Forget the account. Local only — nothing is revoked upstream. */
    fun signOut() {
        store(null)
    }

    /**
     * Run an interactive sign-in, calling [onCode] once the user has somewhere
     * to go, and returning the session when they have approved.
     *
     * Blocks for as long as the code is valid — a quarter of an hour — because
     * that is how long the user has to finish, and the bridge deliberately has
     * no call timeout for exactly this kind of operation.
     */
    suspend fun signIn(onCode: (Core.DeviceCode) -> Unit): JsonObject = withContext(Dispatchers.IO) {
        val code = Core.accountDeviceCodeFrom(exchangeRaw(Core.accountDeviceCodeRequest()))
        onCode(code)

        val msa = awaitApproval(code)
        val session = buildSession(msa)
        store(session)
        session
    }

    /** Open the approval page in the user's own browser. */
    fun openApproval(code: Core.DeviceCode) {
        // Their real browser, not a WebView we control: it carries whatever
        // Microsoft session they already have, which is usually the difference
        // between approving and typing a password on a phone keyboard.
        val intent = Intent(Intent.ACTION_VIEW, Uri.parse(code.approvalUrl))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        runCatching { context.startActivity(intent) }
            .onFailure { Log.w(TAG, "no browser to open the approval page: ${it.message}") }
    }

    // -----------------------------------------------------------------------
    // The chain
    // -----------------------------------------------------------------------

    /**
     * Poll until the user approves, declines, or the code expires.
     *
     * The waiting states arrive as HTTP 400 with an `error` field, which is why
     * every call here goes through [exchangeRaw] and asks the core what the
     * body meant rather than looking at the status.
     *
     * # A failed poll is not a failed sign-in
     *
     * This loop runs for up to a quarter of an hour, on a phone, while the user
     * is in another app entirely — and Android is free to freeze this process
     * while they are. A dropped request in that window is the ordinary case,
     * not the exceptional one, and the first version of this treated one as
     * fatal: a two-second connectivity blip while the user was still reading
     * Microsoft's consent screen ended the sign-in they were halfway through,
     * and they came back to the same button they had already pressed.
     *
     * So a poll that cannot complete is retried. Only three things end this:
     * an answer from Microsoft, the code expiring, or a network that has been
     * gone long enough ([GIVE_UP_AFTER_FAILURES]) that there is no point
     * pretending otherwise.
     */
    private suspend fun awaitApproval(code: Core.DeviceCode): JsonObject {
        val request = Core.accountPollRequest(code.deviceCode)
        val deadline = System.currentTimeMillis() + code.expiresInSecs * 1000
        var interval = code.intervalSecs * 1000
        var consecutiveFailures = 0

        while (System.currentTimeMillis() < deadline) {
            delay(interval)

            val outcome = try {
                Core.accountPollOutcome(exchangeRaw(request)).also { consecutiveFailures = 0 }
            } catch (err: Exception) {
                if (err is CancellationException) throw err
                consecutiveFailures++
                Log.i(
                    TAG,
                    "poll $consecutiveFailures did not get through, still waiting: ${err.message}",
                )
                if (consecutiveFailures >= GIVE_UP_AFTER_FAILURES) {
                    throw AuthException(
                        "Lost the connection while waiting for you to approve the sign-in. " +
                            "Check your connection and try again."
                    )
                }
                continue
            }

            when (outcome["kind"]?.jsonPrimitive?.contentOrNull) {
                "pending" -> Unit
                // Microsoft asking to be polled less often. Obliging is not
                // optional: keep going at the old rate and it starts refusing.
                "slowDown" -> interval += 1000
                "declined" -> throw AuthException("Sign-in was declined.")
                "expired" -> throw AuthException("The sign-in code expired. Please try again.")
                "approved" -> return outcome
                else -> throw AuthException("Microsoft returned something unexpected.")
            }
        }
        throw AuthException("The sign-in code expired. Please try again.")
    }

    /**
     * Everything after the Microsoft token: Xbox Live, XSTS, Minecraft, profile.
     *
     * Shared by sign-in and refresh, because a refreshed MSA token has to walk
     * the identical chain — the Minecraft token is not refreshable on its own.
     */
    private fun buildSession(approved: JsonObject): JsonObject {
        // `approved` is the core's Poll::Approved or an MsaTokens; both carry
        // the same fields, so take them from wherever they landed.
        val msa: JsonElement = approved["fields"] ?: approved

        val xbl = Core.accountXboxTokenFrom(
            exchange(Core.accountXblRequest(str(msa, "accessToken")))
        )

        val xstsResponse = exchangeRaw(Core.accountXstsRequest(str(xbl, "token")))
        // The account-shaped refusals — no Xbox profile, a child account, a
        // region needing verification — all arrive here, and each one names
        // something different for the player to go and fix.
        if (xstsResponse.jsonObject.containsKey("XErr")) {
            throw AuthException(Core.accountXstsRefusal(xstsResponse))
        }
        val xsts = Core.accountXboxTokenFrom(xstsResponse)

        val minecraftToken = Core.accountMinecraftTokenFrom(
            exchange(Core.accountMinecraftLoginRequest(xsts))
        )
        val profile = exchangeRaw(Core.accountProfileRequest(minecraftToken))

        return Core.accountSessionFrom(profile, minecraftToken, msa, System.currentTimeMillis())
    }

    private fun str(value: JsonElement, key: String): String =
        value.jsonObject[key]?.jsonPrimitive?.contentOrNull
            ?: throw AuthException("The sign-in response was missing \"$key\".")

    // -----------------------------------------------------------------------
    // Transport
    // -----------------------------------------------------------------------

    /** Perform a call, failing on a non-2xx. */
    private fun exchange(request: Core.HttpRequest): JsonElement {
        val (status, body) = send(request)
        if (status !in 200..299) {
            // Never the body: these responses carry tokens on the way through.
            throw AuthException("Microsoft rejected the sign-in (HTTP $status).")
        }
        return body
    }

    /**
     * Perform a call and hand back the body whatever the status was.
     *
     * For the three steps where a non-2xx is *information* rather than a
     * failure: a poll that is still waiting, an XSTS refusal naming an account
     * restriction, and a profile lookup that 404s because the account does not
     * own Minecraft. Reading the status alone would turn all three into the
     * same unhelpful error.
     */
    private fun exchangeRaw(request: Core.HttpRequest): JsonElement = send(request).second

    /**
     * Perform a call, retrying a request that never reached Microsoft.
     *
     * Only for the transport failing — a reply with a status, however
     * unwelcome, is an answer and is returned as-is. The retry matters most
     * *after* approval: by then the user has spent their code, and the four
     * remaining calls cannot be restarted without sending them back to
     * Microsoft to approve a second one. Losing a sign-in there to a dropped
     * packet is the difference between a working feature and one that fails a
     * few percent of the time for no visible reason.
     */
    private fun send(request: Core.HttpRequest): Pair<Int, JsonElement> {
        var last: Exception? = null
        repeat(TRANSPORT_ATTEMPTS) { attempt ->
            try {
                return sendOnce(request)
            } catch (err: AuthException) {
                // Reaching Microsoft and being told something is an answer.
                throw err
            } catch (err: IOException) {
                last = err
                Log.i(TAG, "request attempt ${attempt + 1} failed: ${err.message}")
                Thread.sleep(RETRY_BACKOFF_MS * (attempt + 1))
            }
        }
        throw AuthException("Could not reach Microsoft. Check your connection and try again.")
            .also { Log.w(TAG, "giving up after $TRANSPORT_ATTEMPTS attempts: ${last?.message}") }
    }

    private fun sendOnce(request: Core.HttpRequest): Pair<Int, JsonElement> {
        val connection = (URL(request.url).openConnection() as HttpURLConnection).apply {
            requestMethod = request.method
            connectTimeout = CONNECT_TIMEOUT_MS
            readTimeout = READ_TIMEOUT_MS
            setRequestProperty("User-Agent", USER_AGENT)
            request.headers.forEach { (name, value) -> setRequestProperty(name, value) }
            doOutput = request.body != null
        }

        try {
            request.body?.let { body ->
                connection.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
            }
            val status = connection.responseCode
            val stream = if (status in 200..299) connection.inputStream else connection.errorStream
            val text = stream?.bufferedReader()?.use { it.readText() }.orEmpty()
            if (text.isBlank()) {
                throw AuthException("Microsoft returned an empty response (HTTP $status).")
            }
            val body = runCatching { json.parseToJsonElement(text) }.getOrElse {
                // Deliberately not the text — an error page is one thing, but
                // this path also sees token responses.
                throw AuthException("Microsoft returned an unreadable response (HTTP $status).")
            }
            return status to body
        } finally {
            // `IOException` is deliberately not caught here: it means the
            // request never landed, which is the one failure worth trying
            // again, and [send] is what decides how many times.
            connection.disconnect()
        }
    }

    /** A sign-in that failed for a reason worth showing the player. */
    class AuthException(message: String) : Exception(message)

    private companion object {
        const val PREFS = "homerun"
        const val KEY_SESSION = "minecraft_session"

        const val CONNECT_TIMEOUT_MS = 15_000
        const val READ_TIMEOUT_MS = 20_000

        /** Attempts per request before a network failure is called one. */
        const val TRANSPORT_ATTEMPTS = 3
        const val RETRY_BACKOFF_MS = 800L

        /**
         * Consecutive failed polls before a sign-in gives up.
         *
         * Twenty-four at a five-second interval is about two minutes of solid
         * silence — long enough to ride out a lift, a network handover, or the
         * app being frozen while the user is in their browser, and short enough
         * that a genuinely dead connection is not left spinning for the full
         * fifteen.
         */
        const val GIVE_UP_AFTER_FAILURES = 24

        const val USER_AGENT = "Homerun-Android/${BuildConfig.VERSION_NAME}"

        const val TAG = "HomerunMcAuth"
    }
}
