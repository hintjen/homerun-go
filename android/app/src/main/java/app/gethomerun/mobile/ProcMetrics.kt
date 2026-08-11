package app.gethomerun.mobile

import android.system.Os
import android.system.OsConstants
import android.util.Log
import java.io.File

/**
 * What a process is costing, read from `/proc`.
 *
 * The platform half of `homerun_core::metrics`: this reads **counters** and
 * hands them over, and the core decides what they mean. Nothing here computes
 * a percentage — a percentage is a difference between two moments, and getting
 * that arithmetic right is exactly what the core exists to do once instead of
 * three times.
 *
 * # Why `/proc` and not `ActivityManager`
 *
 * The server is a child process. `ActivityManager` and `Debug` report on *this*
 * app, which on a phone hosting a Java server is mostly the WebView — a number
 * that looks like an answer and is about the wrong process.
 * `getProcessMemoryInfo` was the alternative and has been throttled to a few
 * calls a minute since Android Q, which a graph would exhaust.
 *
 * A process can read `/proc/<pid>` for its own children with no permission and
 * no `<queries>` entry, on every API this app supports. What SELinux blocks is
 * *other apps'* processes, which is not what this asks for.
 *
 * Every reader returns null rather than a guess. A phone that will not answer
 * renders "unavailable", which is true; a zero would be a lie on a graph.
 */
object ProcMetrics {

    /**
     * `utime` and `stime` are in clock ticks, and the divisor is a property of
     * the kernel rather than a constant. It is 100 on every Android device seen
     * so far, which is exactly why it should be read rather than assumed —
     * hard-coding it would be right until it silently was not.
     */
    private val ticksPerSecond: Double by lazy {
        runCatching { Os.sysconf(OsConstants._SC_CLK_TCK).toDouble() }
            .getOrNull()
            ?.takeIf { it > 0 }
            ?: 100.0
    }

    /**
     * Resident memory in KiB — what the process actually occupies.
     *
     * From `status` rather than `statm` because it is already in KiB and
     * labelled, where `statm` is in pages and would need the page size and a
     * field index to be right.
     */
    fun residentKb(pid: Long): Long? = read("/proc/$pid/status") { text ->
        text.lineSequence()
            .firstOrNull { it.startsWith("VmRSS:") }
            ?.filter(Char::isDigit)
            ?.takeIf { it.isNotEmpty() }
            ?.toLongOrNull()
    }

    /**
     * Cumulative CPU seconds this process has used, user plus system.
     *
     * Cumulative on purpose: the core turns two of these into a rate. Handing
     * over a rate computed here would put the arithmetic back in the host.
     */
    fun cpuSeconds(pid: Long): Double? = read("/proc/$pid/stat") { text ->
        // Field 2 is the executable name in parentheses and may contain both
        // spaces and parentheses — `(libjavabin.so)` is tame, but a name like
        // `(a) b (c)` is legal. Splitting the whole line is the classic bug
        // here; everything after the *last* `)` is unambiguous.
        val fields = text.substringAfterLast(')').trim().split(' ')
        // `state` is field 3 and lands at index 0, so field N is at N - 3.
        val utime = fields.getOrNull(11)?.toLongOrNull()
        val stime = fields.getOrNull(12)?.toLongOrNull()
        if (utime == null || stime == null) null else (utime + stime) / ticksPerSecond
    }

    /**
     * Read a `/proc` file, or null.
     *
     * These vanish the instant the process exits, and a sampler racing a stop
     * is the normal case rather than an exceptional one — so a failure here is
     * logged at debug and nothing more.
     */
    private fun <T> read(path: String, parse: (String) -> T?): T? = runCatching {
        parse(File(path).readText())
    }.onFailure {
        Log.d(TAG, "could not read $path: ${it.message}")
    }.getOrNull()

    private const val TAG = "HomerunJava"
}
