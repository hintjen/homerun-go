package app.gethomerun.mobile

import android.app.ActivityManager
import android.app.Application
import android.app.ApplicationExitInfo
import android.content.Context
import android.os.Build
import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.io.InputStream

/**
 * Why the last process died, when it died in a way that could not tell us.
 *
 * # The gap this closes
 *
 * [AppErrors] covers everything the app can report about itself: a Kotlin
 * throw, a page error, a Rust panic. All of those run code on the way down.
 * A SIGSEGV in JNI does not. Neither does an ANR, and neither does the kernel
 * reclaiming a process for memory — which in this app is not a corner case at
 * all, because it hosts a Minecraft server on a phone.
 *
 * # Why not a signal handler
 *
 * Because the platform already collects this, and a signal handler is the most
 * dangerous code we could write. It runs on a thread already in an undefined
 * state, everything it calls must be async-signal-safe — no malloc, so no JSON
 * and no Foundation — and a mistake in it turns a crash the OS would have
 * recorded cleanly into a corrupted one. `ApplicationExitInfo` is the same
 * information, gathered by the system, read afterwards on an ordinary thread
 * with the whole language available.
 *
 * # It reports on the next launch, like everything else here
 *
 * The same shape as [AppErrors.drain] and for the same reason: the process
 * that could have spoken is gone. Nothing new was needed to carry it.
 *
 * Android 11 (API 30) and up. That is above this app's `minSdk` of 26, so
 * devices on 8 through 10 report nothing here — they keep the coverage they
 * already had and lose none.
 */
object ExitReasons {

    /**
     * Reasons worth a report.
     *
     * `REASON_CRASH` — an uncaught Java exception — is deliberately absent.
     * [HomerunApplication] already stashes those with a real stack, and
     * reporting them here as well would file every Kotlin crash twice, with
     * the worse copy carrying no stack at all. Duplicates are precisely what
     * the rest of this design spends its effort avoiding.
     *
     * `REASON_SIGNALED` is absent too, but for a weaker reason: it is mostly
     * the shell and the debugger during development, and what it looks like on
     * a real device is unknown. Worth revisiting once there is data.
     */
    private val REPORTED = setOf(
        ApplicationExitInfo.REASON_CRASH_NATIVE,
        ApplicationExitInfo.REASON_ANR,
        ApplicationExitInfo.REASON_LOW_MEMORY,
        ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE,
    )

    /**
     * Read the exit history and report anything new in it.
     *
     * Off the main thread: reading an ANR trace means pulling every thread's
     * stack out of a pipe, and a tombstone can be hundreds of kilobytes.
     */
    fun report(app: Application) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        ServerHost.scope.launch(Dispatchers.IO) {
            runCatching { collect(app) }
                .onFailure { Log.w(TAG, "could not read the exit history: ${it.message}") }
        }
    }

    private fun collect(app: Application) {
        val manager = app.getSystemService(ActivityManager::class.java) ?: return
        val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val mark = prefs.getLong(KEY_MARK, 0L)

        val history = manager.getHistoricalProcessExitReasons(app.packageName, 0, MAX_HISTORY)
        // Oldest first, so the mark below only ever moves forward.
        val fresh = history.filter { it.timestamp > mark }.sortedBy { it.timestamp }
        if (fresh.isEmpty()) return

        // Advanced past everything *examined*, not just everything reported,
        // and written before a single report is built.
        //
        // Both halves are deliberate. Marking only what was reported would
        // re-examine the same uninteresting exits on every launch forever.
        // Marking after reporting would mean a death while reporting a death
        // replays it next launch, and the launch after that, forever — the
        // same loop `error.drain` cuts by deleting a file before it parses it,
        // and this pays the same price for it: a death mid-report is lost.
        prefs.edit().putLong(KEY_MARK, fresh.last().timestamp).apply()

        // Newest first past the cap: if this device died more times than we
        // will report, the recent deaths are the ones worth having.
        val worth = fresh.filter { it.reason in REPORTED }.takeLast(MAX_REPORTS)
        if (worth.isEmpty()) return

        Log.i(TAG, "reporting ${worth.size} process death(s) from before this launch")
        worth.forEach { AppErrors.report(occurrenceOf(it)) }
    }

    private fun occurrenceOf(info: ApplicationExitInfo): JsonObject {
        val kind = kindOf(info)
        return buildJsonObject {
            put("source", AppErrors.SOURCE_NATIVE)
            put("severity", severityOf(info.reason))
            put("kind", kind)
            // The system's own sentence. For an ANR it is the useful half:
            // "Input dispatching timed out" and "Broadcast of Intent { ... }"
            // are different bugs, and the description is what separates them.
            put("message", info.description?.takeIf { it.isNotBlank() } ?: kind)
            excerpt(info)?.let { put("stack", it) }
            // Which process died. Hosting runs in its own process, so "the
            // server died" and "the app died" are different incidents and this
            // is the field that tells them apart.
            put("location", info.processName)
            put("atMs", info.timestamp)
            put(
                "extra",
                buildJsonObject {
                    put("importance", info.importance)
                    put("status", info.status)
                    put("pssKb", info.pss)
                    put("rssKb", info.rss)
                },
            )
        }
    }

    /**
     * The signal rides in `kind`, not in the message.
     *
     * It has to be here or nowhere. `kind` goes into the fingerprint verbatim,
     * while the message is generalised first -- digit runs become `#` -- so
     * "signal 11" and "signal 6" are the same string by the time they are
     * hashed. Without this, and with no stack to fall back on (see [excerpt]),
     * every native death in a process would be one group.
     */
    private fun kindOf(info: ApplicationExitInfo): String = when (info.reason) {
        ApplicationExitInfo.REASON_CRASH_NATIVE -> "native-crash (${signalName(info.status)})"
        ApplicationExitInfo.REASON_ANR -> "anr"
        ApplicationExitInfo.REASON_LOW_MEMORY -> "low-memory"
        ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE -> "excessive-resource-usage"
        else -> "exit-${info.reason}"
    }

    /** For [REASON_CRASH_NATIVE][ApplicationExitInfo.REASON_CRASH_NATIVE], `status` is the signal. */
    private fun signalName(status: Int): String = when (status) {
        4 -> "SIGILL"
        6 -> "SIGABRT"
        7 -> "SIGBUS"
        8 -> "SIGFPE"
        11 -> "SIGSEGV"
        5 -> "SIGTRAP"
        else -> "signal $status"
    }

    /**
     * A memory kill is not a failure of the app's own logic — the system
     * reclaimed it, and it may have been behaving perfectly. Recording it as
     * `error` keeps `fatal` meaning "this app broke".
     */
    private fun severityOf(reason: Int): String = when (reason) {
        ApplicationExitInfo.REASON_CRASH_NATIVE,
        ApplicationExitInfo.REASON_ANR,
        -> AppErrors.SEVERITY_FATAL
        else -> AppErrors.SEVERITY_ERROR
    }

    /**
     * The part of the trace worth sending, in the shape the core already
     * parses.
     *
     * Neither trace can be forwarded whole. An ANR dump is every thread in the
     * process and routinely runs past a hundred kilobytes; a tombstone carries
     * registers, memory maps and a disassembly window. The core caps a stack at
     * 8 KiB, so sending the raw thing means sending the header and discarding
     * the frames — exactly backwards.
     */
    private fun excerpt(info: ApplicationExitInfo): String? {
        val raw = runCatching { info.traceInputStream?.use { read(it) } }
            .onFailure { Log.w(TAG, "could not read the trace: ${it.message}") }
            .getOrNull()
            ?: return null

        if (!looksTextual(raw)) {
            // Android 12 and up store a tombstone as a protobuf, so this is
            // where a native crash ends up: bytes we have no schema for. Said
            // out loud rather than silently producing an empty stack, because
            // "no frames" and "frames we threw away" look identical in a table.
            Log.i(TAG, "trace for ${kindOf(info)} is not text (${raw.size} bytes); no stack sent")
            return null
        }

        val text = String(raw, Charsets.UTF_8)

        return when (info.reason) {
            ApplicationExitInfo.REASON_ANR -> mainThread(text.lineSequence())
            ApplicationExitInfo.REASON_CRASH_NATIVE -> backtrace(text.lineSequence())
            else -> null
        }?.takeIf { it.isNotBlank() }
    }

    /**
     * The main thread's stack out of an ANR dump.
     *
     * Every thread is in there and only one of them is the answer — and it is
     * nearly always this one, because an ANR *is* the main thread failing to
     * return. Sending all of them would blow the cap, and would also
     * fingerprint on whichever thread happened to be dumped first, which is
     * not stable between runs.
     */
    private fun mainThread(lines: Sequence<String>): String {
        val out = StringBuilder()
        var inMain = false
        for (line in lines) {
            // A quoted name at the start of a line opens a thread block.
            if (line.startsWith("\"")) {
                if (inMain) break
                inMain = line.startsWith("\"main\"")
            }
            if (inMain) out.append(line).append('\n')
            if (out.length > MAX_EXCERPT) break
        }
        return out.toString()
    }

    /**
     * The signal line and the crashing thread's frames out of a tombstone,
     * rewritten as `at symbol (library)`.
     *
     * That shape is not cosmetic: it is what the core's frame parser already
     * reads for every other language, so a native crash groups by the same
     * rules as everything else rather than needing a second parser that would
     * drift from the first.
     */
    private fun backtrace(lines: Sequence<String>): String {
        val out = StringBuilder()
        var inBacktrace = false
        for (line in lines) {
            val trimmed = line.trim()
            // Which signal, and at what address. One line, and the most
            // informative line in the file.
            if (!inBacktrace && trimmed.startsWith("signal ")) {
                out.append(trimmed).append('\n')
            }
            if (trimmed.startsWith("backtrace:")) {
                inBacktrace = true
                continue
            }
            if (inBacktrace) {
                val frame = nativeFrame(trimmed)
                if (frame == null) {
                    // Blank lines and headings before the first frame are
                    // skipped; anything after it ends the backtrace.
                    if (out.isEmpty()) continue else break
                }
                out.append(frame).append('\n')
                if (out.length > MAX_EXCERPT) break
            }
        }
        return out.toString()
    }

    /**
     * `#01 pc 0000000000049b3c  /path/to/libfoo.so (some_symbol+164) (BuildId: ab12)`
     * becomes `    at some_symbol (libfoo.so)`.
     *
     * The offset goes with the address. `+164` is how far into the function it
     * stopped, which moves whenever the library is recompiled — keeping it
     * would give one bug a new fingerprint on every release, which is a failure
     * a rebuilt UI bundle has already demonstrated once.
     */
    private fun nativeFrame(line: String): String? {
        if (!line.startsWith("#")) return null
        val at = line.indexOf(" /")
        if (at < 0) return null
        val rest = line.substring(at + 1)
        val library = rest.substringBefore(' ').substringAfterLast('/')
        val open = rest.indexOf('(')
        val symbol = if (open < 0) "?" else rest.substring(open + 1).substringBefore(')')
        return "    at ${symbol.substringBefore('+')} ($library)"
    }

    /**
     * Printable enough to parse as a stack.
     *
     * An ANR dump is text; a modern tombstone is a protobuf. Both arrive
     * through the same call and nothing in the API says which one this is, so
     * the bytes have to be asked. A NUL byte settles it -- UTF-8 text has
     * none, and protobuf is full of them.
     */
    private fun looksTextual(bytes: ByteArray): Boolean {
        val sample = minOf(bytes.size, 512)
        if (sample == 0) return false
        return (0 until sample).none { bytes[it] == 0.toByte() }
    }

    /** Bounded on purpose: a tombstone is not required to be small. */
    private fun read(stream: InputStream): ByteArray {
        val buffer = ByteArray(MAX_TRACE)
        var filled = 0
        while (filled < MAX_TRACE) {
            val n = stream.read(buffer, filled, MAX_TRACE - filled)
            if (n <= 0) break
            filled += n
        }
        return buffer.copyOf(filled)
    }

    private const val TAG = "HomerunExitReasons"
    private const val PREFS = "homerun-host"
    private const val KEY_MARK = "exit-reasons-through"

    /** How far back to look. The system keeps a short history of its own. */
    private const val MAX_HISTORY = 16

    /** Matches `errors::MAX_DRAIN`, and for the same reason. */
    private const val MAX_REPORTS = 5

    /** Read out of the pipe, before anything is picked from it. */
    private const val MAX_TRACE = 256 * 1024

    /** Sent, and comfortably under the core's 8 KiB cap on a stack. */
    private const val MAX_EXCERPT = 6 * 1024
}
