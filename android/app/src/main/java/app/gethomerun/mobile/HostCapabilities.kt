package app.gethomerun.mobile

import kotlinx.serialization.Serializable

/**
 * What this host supports, as the shared UI reads it.
 *
 * Mirrors `HostCapabilities` in homerun-app-ui (`lib/bridge/capabilities.ts`).
 * The UI resolves this **once, synchronously, at startup** and cannot await
 * it, so it is injected at document start (PROTOCOL.md §4.1).
 *
 * Every field must be present. A missing field is a host bug, not a default —
 * the UI reads it as `undefined` and capability gates silently take the wrong
 * branch.
 */
@Serializable
data class HostCapabilities(
    val platform: String,
    val serverBackends: List<String>,
    val installation: Boolean,
    val moveInstallation: Boolean,
    val clientLauncher: Boolean,
    val windowChrome: Boolean,
    val tray: Boolean,
    val autoUpdate: Boolean,
    val privilegeElevation: Boolean,
    val moddedServers: Boolean,
    val fileImport: Boolean,
    val multipleRunningServers: Boolean,
    val backgroundExecution: Boolean,
    val backups: Boolean,
    val deviceWebsocket: Boolean,
) {
    companion object {
        /**
         * The Android profile. Kept identical to `ANDROID_PREVIEW_CAPABILITIES`
         * in the UI repo — that constant is what the conformance manifest was
         * generated from, so drift here means the UI calls channels this host
         * never implements.
         *
         * These describe the *platform*, not today's progress. Android can run
         * a JVM, so `moddedServers` is true and mod/plugin UI stays visible
         * even while the backend that serves it is still being built.
         */
        val ANDROID = HostCapabilities(
            platform = "android",
            serverBackends = listOf("javaNative", "pumpkin"),
            installation = false,
            moveInstallation = false,
            clientLauncher = false,
            windowChrome = false,
            tray = false,
            autoUpdate = false,
            privilegeElevation = false,
            // The JVM backend runs Bukkit-family plugins and Forge/Fabric mods.
            moddedServers = true,
            // Mods and modpacks come from the in-app browser. Being handed a
            // file — a world .zip, a .mrpack — is a desktop flow, and SAF
            // import is deliberately not being built.
            fileImport = false,
            // One server at a time, same as desktop.
            multipleRunningServers = false,
            // Implemented, not aspirational: a foreground service holds the
            // process at foreground importance while a server runs, and past
            // the stop until the on-stop backup has finished uploading. See
            // `docs/android-lifecycle.md`.
            backgroundExecution = true,
            // restic, being wired now. Left true because that work is in
            // flight — but until it lands the UI offers a feature that does
            // nothing, and, the part that costs data, a world played here
            // never becomes the newest snapshot, so the next desktop launch
            // restores over it.
            backups = true,
            // Both reasons this was false have gone. `get-device-ws-port`
            // answers a real port that a websocket is listening on, and the
            // foreground service means the socket outlives the app being put
            // away rather than existing only while somebody is looking at it.
            //
            // The contract has said `true` for Android all along — this
            // constant was the one out of step, which is the drift the note
            // above warns about, pointing the other way. See
            // `plans/device-websocket.md`.
            deviceWebsocket = true,
        )
    }
}
