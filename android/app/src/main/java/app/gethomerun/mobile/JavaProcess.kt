package app.gethomerun.mobile

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.add
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Running a JVM on Android — for the server, and for anything else.
 *
 * # Why this is not just the server backend
 *
 * Every mod loader in `plans/android-mod-loaders.md` installs by **running an
 * installer jar**: Fabric's writes `fabric-server-launch.jar`, NeoForge's
 * fetches libraries and generates the argfiles that launch it. Those are JVM
 * launches that are not the server, they need the same platform knowledge, and
 * before this file that knowledge lived inline in [JavaServerBackend] where
 * only a server launch could reach it.
 *
 * So the platform-specific part is here and has one caller shape:
 *
 *  - [invocation] composes a launch — which launcher may be exec'd, which
 *    `libjvm.so` was unpacked, and what a Termux-built runtime needs on
 *    `LD_LIBRARY_PATH` before the linker reads it at exec.
 *  - [run] executes one to completion and returns its exit code, for the
 *    launches nobody supervises.
 *
 * A **server** launch does not use [run]. It is handed to the supervisor in
 * `homerun-pumpkin-ffi`, which owns the console, the stop ladder and the
 * meaning of an exit — the same state machine that runs the linked engine on
 * iOS. [run] is for the short, unsupervised launches: start it, read what it
 * says, wait for it to stop.
 *
 * # The launcher contract
 *
 * ```text
 * libjavabin.so <libjvm.so> <main-class> [jvm-option ...] -- [program arg ...]
 * ```
 *
 * There is no `-jar`. The VM is created through JNI — `JNI_CreateJavaVM`
 * takes options directly — so the jar goes on the classpath and the main class
 * is named separately. `rust/homerun-java-launcher/src/main.rs` is the other
 * half of this contract.
 *
 * That JNI detail is also why an `@argfile` cannot simply be passed through:
 * expanding one is a feature of the `java` *launcher binary*, and there is no
 * `java` binary here. Forge and NeoForge launch entirely through argfiles, and
 * expanding them is M3's job.
 */
object JavaProcess {

    /** A composed launch: what to exec, with what, in what environment. */
    data class Invocation(
        val program: String,
        val args: List<String>,
        val env: Map<String, String>,
        val workDir: File,
    ) {
        /** The shape the supervisor in `homerun-pumpkin-ffi` reads. */
        fun toJson(): JsonObject = buildJsonObject {
            put("program", program)
            put("args", buildJsonArray { args.forEach { add(it) } })
            put("env", buildJsonObject { env.forEach { (k, v) -> put(k, v) } })
        }
    }

    /**
     * Compose a JVM launch.
     *
     * [jvmOptions] and [programArgs] are the caller's — for a server they come
     * from `homerun-core` (`jvm::launch`), because what a Minecraft server is
     * given is not an Android question. Everything this function adds is,
     * which is exactly why it is the part that is shared.
     *
     * [classpath] is joined with `:`; Android is Unix and the JVM here is
     * Termux's. A single-element list is the common case, and an **empty** one
     * is legitimate: Forge and NeoForge launch from a module path their
     * argfile supplies, and putting a jar on the class path beside it would
     * load the same classes twice.
     */
    fun invocation(
        launcher: File,
        javaHome: File,
        libjvm: File,
        classpath: List<File>,
        mainClass: String,
        jvmOptions: List<String> = emptyList(),
        programArgs: List<String> = emptyList(),
        workDir: File,
        tmpDir: File,
        extraEnv: Map<String, String> = emptyMap(),
    ): Invocation {
        val args = buildList {
            add(libjvm.absolutePath)
            // The launcher normalises this too; doing it here keeps what is
            // exec'd identical to what this file says it exec'd.
            add(mainClass.replace('.', '/'))
            if (classpath.isNotEmpty()) {
                add("-Djava.class.path=${classpath.joinToString(":") { it.absolutePath }}")
            }
            add("-Djava.home=${javaHome.absolutePath}")
            // The JRE's own natives live here; without it the VM starts but
            // java.nio cannot load libnio.so.
            add("-Djava.library.path=${javaHome.absolutePath}/lib")
            // These builds are Termux's and carry Termux's prefix compiled in
            // as the temp directory — a path that does not exist outside
            // Termux, so anything writing a temp file fails on a path no one
            // can explain. An installer writes a great many temp files.
            add("-Djava.io.tmpdir=${tmpDir.absolutePath}")
            add("-Duser.dir=${workDir.absolutePath}")
            addAll(jvmOptions)
            add("--")
            addAll(programArgs)
        }

        val env = buildMap {
            put("JAVA_HOME", javaHome.absolutePath)
            // The runtime's .so files carry DT_NEEDED entries for each other
            // (libnio -> libnet) and Android's linker will not find them
            // without this. It has to be in the environment: the linker reads
            // it at process start, so setting it later is too late.
            put(
                "LD_LIBRARY_PATH",
                listOfNotNull(
                    "${javaHome.absolutePath}/lib",
                    "${javaHome.absolutePath}/lib/server",
                    // Termux's libandroid-shmem, libandroid-spawn and
                    // libz.so.1. The runtime's DT_RUNPATH points at Termux's
                    // own prefix, which does not exist here — LD_LIBRARY_PATH
                    // is searched first, so this resolves.
                    "${javaHome.absolutePath}/${JavaRuntime.DEPS_DIR}",
                    System.getenv("LD_LIBRARY_PATH"),
                ).joinToString(":"),
            )
            put("HOME", workDir.absolutePath)
            putAll(extraEnv)
        }

        return Invocation(launcher.absolutePath, args, env, workDir)
    }

    /**
     * Run [invocation] to completion and return its exit code.
     *
     * Output is merged and streamed to [onLog] a line at a time, so a slow
     * installer shows progress in the console the player is already reading
     * rather than going quiet for minutes.
     *
     * **Cancellable.** A stop during a loader install must take effect at once
     * rather than after the download finishes, so cancelling the calling
     * coroutine destroys the process. That is the desktop's behaviour too —
     * `setupServerLoader` races every step against the launch's `AbortController`.
     *
     * [timeoutMs] is a backstop for a wedged installer, not a budget: the
     * desktop allows Forge ten minutes and a phone on mobile data is slower,
     * so keep it generous. It is not a bridge timeout — `native-server-start`
     * still has none.
     */
    suspend fun run(
        invocation: Invocation,
        timeoutMs: Long,
        onLog: (String) -> Unit,
    ): Int = withContext(Dispatchers.IO) {
        val builder = ProcessBuilder(listOf(invocation.program) + invocation.args)
            .directory(invocation.workDir)
            // One stream. Installers write progress to both and interleaving
            // them by hand would only reorder what the user reads.
            .redirectErrorStream(true)
        builder.environment().putAll(invocation.env)

        val process = builder.start()
        // Bounded, and kept only for the failure message — `onLog` has already
        // streamed everything live. A BuildTools-scale compile emits tens of
        // megabytes, so this holds the diagnostic tail rather than all of it.
        val tail = ArrayDeque<String>()

        try {
            process.inputStream.bufferedReader().use { reader ->
                while (true) {
                    val line = reader.readLine() ?: break
                    // The launcher announces its own pid on the first line so
                    // the host can sample `/proc/<pid>`. Nothing supervises an
                    // unsupervised run, so to a reader it is only noise.
                    if (line.startsWith("[launcher] pid=")) continue
                    if (line.isNotBlank()) {
                        onLog(line)
                        tail.addLast(line)
                        while (tail.size > MAX_TAIL_LINES) tail.removeFirst()
                    }
                }
            }

            if (!process.waitFor(timeoutMs, TimeUnit.MILLISECONDS)) {
                throw ServerBackendException.Engine(
                    "The installer did not finish within ${timeoutMs / 1000}s and was stopped."
                )
            }
            process.exitValue()
        } finally {
            // Covers all three ways out: the timeout above, a cancelled
            // coroutine, and an exception from the reader. A JVM left running
            // here would hold the server directory it was writing into.
            if (process.isAlive) {
                Log.w(TAG, "destroying a Java process that outlived its run")
                process.destroyForcibly()
            }
        }
    }

    /** [run], failing with the tail of the output when the exit code is not 0. */
    suspend fun runOrThrow(
        invocation: Invocation,
        timeoutMs: Long,
        what: String,
        onLog: (String) -> Unit,
    ) {
        val failures = ArrayDeque<String>()
        val code = run(invocation, timeoutMs) { line ->
            onLog(line)
            failures.addLast(line)
            while (failures.size > MAX_TAIL_LINES) failures.removeFirst()
        }
        if (code != 0) {
            throw ServerBackendException.Engine(
                "$what failed (exit $code)" +
                    if (failures.isEmpty()) "." else ":\n${failures.joinToString("\n")}"
            )
        }
    }

    /**
     * The class a jar declares it starts at.
     *
     * Read here so the launcher stays free of zip parsing — it is handed a
     * class name and nothing else. Every jar this app launches has one: a
     * server jar, and every loader installer.
     */
    fun mainClassOf(jar: File): String? = runCatching {
        java.util.jar.JarFile(jar).use { it.manifest?.mainAttributes?.getValue("Main-Class") }
    }.getOrNull()

    /**
     * Enough of a failure to diagnose one, and not so much that it cannot be
     * shown. A stack trace is tens of lines; a Gradle-scale log is millions.
     */
    private const val MAX_TAIL_LINES = 40

    private const val TAG = "HomerunJava"
}
