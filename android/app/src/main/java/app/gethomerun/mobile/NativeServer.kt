package app.gethomerun.mobile

import android.util.Log

/**
 * The raw JNI surface of `homerun-pumpkin-ffi`.
 *
 * Thin on purpose: every method returns the JSON string the Rust layer
 * produced, and nothing here interprets it. [PumpkinBackend] owns the
 * lifecycle and the parsing; this is only the wire.
 *
 * See `docs/ffi.md` for the contract these answers follow.
 */
object NativeServer {

    /**
     * Matches `FFI_ABI_VERSION` in the crate.
     *
     * Kept honest by `scripts/check-abi.js`, which `npm test` runs. It has to
     * be: this sat at 1 while the crate went to 2 and then 3, and nothing
     * noticed, because the check below only runs when something first touches
     * this object — and for a while nothing did.
     */
    private const val EXPECTED_ABI = 3

    /**
     * The engine overflows a default thread stack and takes the process down
     * with no panic report, so [startBlocking] runs on a thread sized for it.
     * The value is from the crate's own host-integration notes.
     */
    private const val ENGINE_STACK_BYTES = 16L * 1024 * 1024

    val available: Boolean

    init {
        var loaded = false
        try {
            System.loadLibrary("homerun_pumpkin_ffi")
            nativeInitLogging()
            val abi = nativeAbiVersion()
            if (abi != EXPECTED_ABI) {
                // Loud rather than fatal: the app still runs, just without a
                // server backend, and the log says exactly why.
                Log.e(TAG, "FFI ABI $abi does not match expected $EXPECTED_ABI — rebuild the Rust crate")
            } else {
                loaded = true
            }
        } catch (err: UnsatisfiedLinkError) {
            Log.e(TAG, "could not load libhomerun_pumpkin_ffi.so — run `npm run rust:android`", err)
        }
        available = loaded
    }

    /**
     * Runs the server to completion on a dedicated, large-stack thread.
     *
     * `nativeStart` blocks for the server's entire lifetime, so this returns
     * the [Thread] immediately; [onExit] fires with the final JSON when the
     * server stops or crashes.
     *
     * [invocation] chooses what to supervise. Null runs the engine linked into
     * the library — Pumpkin. A JSON `Invocation` runs a **child process**,
     * which is how a real Java server is hosted: the same state machine, log
     * buffer and crash handling either way, because the supervisor cannot tell
     * them apart.
     */
    fun startBlocking(
        serverId: String,
        dataDir: String,
        port: Int,
        invocation: String? = null,
        onExit: (String) -> Unit,
    ): Thread {
        val thread = Thread(
            null,
            {
                val result = try {
                    nativeStart(serverId, dataDir, port, invocation)
                } catch (err: Throwable) {
                    """{"ok":false,"error":"${err.message ?: "engine thread failed"}"}"""
                }
                onExit(result)
            },
            "homerun-engine",
            ENGINE_STACK_BYTES,
        )
        thread.isDaemon = true
        thread.start()
        return thread
    }

    private external fun nativeInitLogging()
    external fun nativeAbiVersion(): Int

    /** Blocks until the server stops. Use [startBlocking]. */
    private external fun nativeStart(
        serverId: String,
        dataDir: String,
        port: Int,
        invocation: String?,
    ): String

    external fun nativeStop(): String
    external fun nativeState(): String
    external fun nativeStats(): String
    external fun nativePlayers(): String
    external fun nativeLogsSince(cursor: Long): String
    external fun nativeCommand(command: String): String

    private const val TAG = "HomerunNative"
}
