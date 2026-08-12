package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.onTimeout
import kotlinx.coroutines.selects.select
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.time.Instant
import java.time.format.DateTimeFormatter

/**
 * What this device tells the API about the server it is running.
 *
 * The desktop has reported for years — crashes, stats, presence, minigame
 * results — and this host reported its heartbeat, its state and its backups
 * and nothing else. None of it is needed to *run* a server, which is exactly
 * why the gap survived so long: a host that never reports looks fine.
 *
 * What it cost, concretely: a crashed server gave the player no explanation
 * and support no logs, the dashboard's graphs stayed empty for a phone, and a
 * session shorter than the reporting interval was invisible to journeys
 * because no sample ever caught anybody online.
 *
 * # Where the decisions are
 *
 * Not here. `homerun-core::reporting` decides what a crash log means, what
 * each payload contains, how often to send and when a join is worth an early
 * report; this object performs what it is told and supplies the things only a
 * platform can know — the clock, the process's memory, the console.
 *
 * See `docs/android-reporting.md`.
 *
 * # Process-scoped, like everything that outlives a page
 *
 * A reload must not silence reporting for a server that is still running, and
 * two activities must not report the same server twice. Hosting is one server
 * at a time ([HostCapabilities.ANDROID] declares `multipleRunningServers`
 * false), so the state below is single-valued rather than a map — the same
 * assumption [HostingService] makes.
 */
object Reporting : ServerHost.Listener {

    /**
     * The console, kept here rather than read back on demand.
     *
     * A backend answers `logs()` only while it owns a run: the exit path nulls
     * its current server id immediately after announcing the state, so a crash
     * listener that reacts to `CRASHED` and *then* asks for the console gets
     * an empty list. The desktop keeps `sessionLogs` for the same reason.
     */
    private val tail = ArrayDeque<String>()

    /** Matches the desktop's `SESSION_LOG_LIMIT`; the core caps what it sends. */
    private const val TAIL_LIMIT = 2000

    private var runningId: String? = null
    private var context: RunContext? = null

    /** Opaque cadence state from the core, held between polls. */
    private var schedule: JsonObject? = null
    private var loop: Job? = null

    /**
     * Wakes the reporting loop early.
     *
     * Conflated: a party arriving together is several joins and one wake-up,
     * which is the same coalescing the core's debounce expresses in time.
     */
    private val nudge = Channel<Unit>(Channel.CONFLATED)

    /** Corrupt-jar retries this run. The core owns what the number means. */
    private var retriesUsed = 0

    /**
     * Scrapes in flight. Each answers whether a line was the reply it wanted,
     * which is both how it receives its answer and how [consume] knows to keep
     * the line off the player's console.
     */
    private val scrapes = mutableListOf<(String) -> Boolean>()

    /**
     * What a run needs that only the launch knew.
     *
     * Set by the bridge when a start begins, because the settings are fetched
     * there — the access token that reads them must never reach the backend.
     */
    data class RunContext(
        val loader: String,
        val onlineMode: Boolean?,
        /** `<gateway host>:<external port>`, once the gateway has assigned one. */
        val gatewayAddress: String? = null,
    )

    fun init() {
        ServerHost.addListener(this)
    }

    /**
     * A launch is starting, with what the bridge already fetched.
     *
     * Called before the backend starts, so a crash during startup still has a
     * context to be reported against.
     */
    fun starting(serverId: String, settings: HomerunApi.ServerSettings?) {
        synchronized(this) {
            tail.clear()
            retriesUsed = 0
            context = RunContext(
                loader = settings?.loader ?: "vanilla",
                onlineMode = settings?.let { runCatching { Core.onlineMode(it) }.getOrNull() },
            )
        }
        Log.i(TAG, "reporting armed for $serverId")
    }

    /**
     * The gateway has assigned this server's external port.
     *
     * Arrives from the post-launch tunnel poll rather than from [starting]:
     * the port does not exist yet when a launch begins. Until it does,
     * `gateway_ping` is reported null, which is what the desktop sends in the
     * same window.
     */
    fun gatewayAddressResolved(serverId: String, address: String) {
        synchronized(this) {
            val current = context ?: return
            if (current.gatewayAddress == address) return
            context = current.copy(gatewayAddress = address)
        }
        Log.i(TAG, "$serverId is reachable at $address")
    }

    // -----------------------------------------------------------------------
    // Listening
    // -----------------------------------------------------------------------

    override fun onLog(serverId: String, line: String) {
        val waiting = synchronized(this) {
            tail.addLast(line)
            while (tail.size > TAIL_LIMIT) tail.removeFirst()
            scrapes.toList()
        }
        // Outside the lock: answering a scrape resumes a coroutine.
        //
        // Note what this does *not* do — it does not stop the line reaching
        // the player. The UI pages the console out of the supervisor's own
        // buffer rather than off this event stream, so a reply to a poll made
        // here is visible to whoever is watching. See `docs/android-reporting.md`.
        waiting.forEach { runCatching { it(line) } }

        if (serverId != runningId) return

        // A plugin announcing a finished match. Fire and forget — the core
        // refuses anything that did not come from the server itself.
        Core.minigameReport(serverId, line)?.let { send(it, userSigned = false) }

        // A join or a leave earns an early report, because a session shorter
        // than the interval would otherwise never be seen at all.
        val meaning = runCatching { Core.classify(line) }.getOrNull() ?: return
        if (meaning.joined != null || meaning.left != null) {
            synchronized(this) {
                schedule = Core.schedule(schedule, now(), event = "presence").held
            }
            nudge.trySend(Unit)
        }
    }

    override fun onStateChanged(serverId: String, state: ServerState, backupInProgress: Boolean) {
        when (state) {
            ServerState.RUNNING -> start(serverId)
            ServerState.CRASHED -> {
                stop()
                reportCrash(serverId)
            }
            ServerState.STOPPED -> stop()
            else -> Unit
        }
    }

    // -----------------------------------------------------------------------
    // The loop
    // -----------------------------------------------------------------------

    private fun start(serverId: String) {
        synchronized(this) {
            if (runningId == serverId && loop?.isActive == true) return
            runningId = serverId
            schedule = null
        }
        loop?.cancel()
        loop = ServerHost.scope.launch {
            while (isActive) {
                val decision = synchronized(this@Reporting) {
                    Core.schedule(schedule, now()).also { schedule = it.held }
                }
                if (decision.trigger != null) {
                    runCatching { report(serverId, decision.trigger) }
                        .onFailure { Log.w(TAG, "stats report failed: ${it.message}") }
                    continue
                }
                // Sleep until the core says something is due, unless a join or
                // a leave arrives first.
                select {
                    nudge.onReceive {}
                    onTimeout(decision.waitMs.coerceAtLeast(MIN_WAIT_MS)) {}
                }
            }
        }
    }

    private fun stop() {
        loop?.cancel()
        loop = null
        synchronized(this) {
            runningId = null
            schedule = null
        }
    }

    // -----------------------------------------------------------------------
    // The reports
    // -----------------------------------------------------------------------

    private suspend fun report(serverId: String, trigger: String) {
        val registration = DeviceRegistry.current() ?: return
        val backend = ServerHost.backend
        // The run can end during the round trips below. Reporting a stopped
        // server as running would keep it on the dashboard until something
        // else corrected it.
        if (serverId !in backend.runningServerIds) return

        val run = synchronized(this) { context }
        val loader = run?.loader ?: "vanilla"

        val roster = scrape(serverId, Core.pinned(LIST_UUIDS, loader)) { Core.parseRoster(it) }
        val age = scrape(serverId, Core.pinned(GAMETIME, loader)) { Core.parseServerAge(it) }
        val cpu = backend.cpuUsage(serverId)?.let {
            Core.cpuPercentOfDevice(it, Runtime.getRuntime().availableProcessors())
        }
        val ping = run?.gatewayAddress?.let { Core.gatewayPing(it) }

        if (serverId !in backend.runningServerIds) return

        val stats = buildJsonObject {
            backend.uptime(serverId)?.let { put("serverRuntime", ISO.format(it)) }
            backend.memoryUsage(serverId)?.usedKb?.let { put("memoryKb", it) }
            cpu?.let { put("cpuPercent", it) }
            roster?.let { put("roster", it) }
            run?.onlineMode?.let { put("onlineMode", it) }
            age?.let { put("serverAgeSecs", it) }
            HomerunApi.publicIpAddress()?.let { put("hostIp", it) }
            ping?.let { put("gatewayPingMs", it) }
        }

        Core.statsReport(serverId, registration.deviceId, stats)?.let {
            send(it, userSigned = false)
            // Names what went, not just that something did. A report of all
            // nulls is indistinguishable from a healthy one in a log that only
            // says "reported", and every field here can independently fail to
            // be collected.
            Log.i(
                TAG,
                "reported $serverId ($trigger): " +
                    "players=${roster?.get("count") ?: "?"} " +
                    "age=${age?.toLong() ?: "?"}s " +
                    "cpu=${cpu?.let { c -> "%.1f".format(c) } ?: "?"}% " +
                    "ping=${ping?.toLong() ?: "?"}ms",
            )
        }
    }

    private fun reportCrash(serverId: String) {
        val registration = DeviceRegistry.current() ?: return
        val lines = synchronized(this) { tail.toList() }
        if (lines.isEmpty()) {
            Log.w(TAG, "$serverId crashed with an empty console — nothing to report")
            return
        }

        // The local reading first: it is what the player sees while the API is
        // still deciding, and on a phone the request may not go out for a
        // while.
        val diagnosis = runCatching { Core.crashDiagnosis(lines, retriesUsed) }.getOrNull()
        diagnosis?.let {
            Log.i(TAG, "$serverId crashed: ${it.cause}")
            ServerHost.note(serverId, it.message)
        }

        Core.crashReport(serverId, registration.deviceId, lines)?.let {
            send(it, userSigned = false)
        }
    }

    /**
     * A command a person typed into the console.
     *
     * `ops.json`, `whitelist.json` and `banned-players.json` are rewritten
     * from the API's environment variables at every launch, so an operator
     * granted only here would silently lose op on the next start. Mirroring it
     * back is what makes the change outlive the session — and, because bans
     * travel with the server, follow it to whichever device hosts it next.
     *
     * Serialised, and that is not incidental: this is a read-modify-write
     * against a list, and two commands typed in quick succession would
     * otherwise each read the list before either had written it, so the second
     * would erase the first.
     */
    fun consoleCommand(serverId: String, command: String) {
        val parsed = runCatching { Core.opsCommand(command) }
            .onFailure { Log.w(TAG, "could not read the command ${command.take(24)}: ${it.message}") }
            .getOrNull()
        if (parsed == null) {
            Log.i(TAG, "not an ops command, nothing to sync: ${command.take(24)}")
            return
        }
        opsChain = ServerHost.scope.launch {
            runCatching { opsChain?.join() }
            runCatching { syncOps(serverId, parsed) }
                .onFailure { Log.w(TAG, "ops sync failed for $serverId: ${it.message}") }
        }
    }

    private var opsChain: Job? = null

    private suspend fun syncOps(serverId: String, command: JsonObject) {
        // Deliberately not falling back to the device token. The API would
        // accept it and strip the change, which reads as success and is not.
        val token = Session.userToken() ?: run {
            Log.i(TAG, "nobody is signed in — $serverId keeps this change only until it restarts")
            return
        }
        val api = DeviceRegistry.apiUrl()

        val body = HomerunApi.serverBody(api, serverId, token) ?: return
        val change = Core.opsSync(command, body, serverId) ?: return // already in that state

        val reply = HomerunApi.perform(api, change.request, token)
        if (reply == null) {
            Log.w(TAG, "the API did not accept the change for $serverId")
            return
        }
        // Only after it is saved. Telling a player their change was stored and
        // then losing it on the next launch is worse than saying nothing.
        ServerHost.note(serverId, change.line)
    }

    /**
     * Run a console command and wait for the reply the core can read.
     *
     * `native-server-rcon` is fire-and-forget here — this host's RCON replies
     * arrive as ordinary console lines rather than as a response — so the
     * reply is recognised by the core parsing it, not by matching text. That
     * is the same trick the desktop uses for Bedrock, which has no RCON at
     * all, and it means the shape of a `list uuids` reply is written down once.
     */
    private suspend fun <T> scrape(
        serverId: String,
        command: String,
        parse: (String) -> T?,
    ): T? {
        val answer = Channel<T>(Channel.CONFLATED)
        val watcher: (String) -> Boolean = { line ->
            val parsed = parse(line)
            if (parsed != null) {
                answer.trySend(parsed)
                true
            } else {
                false
            }
        }
        synchronized(this) { scrapes.add(watcher) }
        return try {
            runCatching { ServerHost.backend.command(serverId, command) }
                .onFailure { return null }
            select {
                answer.onReceive { it }
                onTimeout(SCRAPE_TIMEOUT_MS) { null }
            }
        } finally {
            synchronized(this) { scrapes.remove(watcher) }
            answer.close()
        }
    }

    /** Perform a request the core built, with the credential it asked for. */
    private fun send(request: Core.Request, userSigned: Boolean) {
        ServerHost.scope.launch {
            val token = if (userSigned || request.userSigned) {
                Session.userToken()
            } else {
                DeviceRegistry.current()?.deviceToken
            }
            if (token.isNullOrBlank()) {
                Log.w(TAG, "no credential for ${request.path} — not sent")
                return@launch
            }
            HomerunApi.perform(DeviceRegistry.apiUrl(), request, token)
        }
    }

    private fun now(): Long = System.currentTimeMillis()

    private const val TAG = "HomerunReporting"

    /** Vanilla since 1.8.1 — names and UUIDs in one round trip. */
    private const val LIST_UUIDS = "list uuids"
    private const val GAMETIME = "time query gametime"

    /** A console reply arrives in well under a second, or not at all. */
    private const val SCRAPE_TIMEOUT_MS = 3_000L

    /** Never spin, even if the core ever answered that something is due now. */
    private const val MIN_WAIT_MS = 200L

    private val ISO: DateTimeFormatter = DateTimeFormatter.ISO_INSTANT
}
