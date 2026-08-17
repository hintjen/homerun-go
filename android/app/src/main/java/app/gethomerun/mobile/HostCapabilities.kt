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
    val minecraftAccount: Boolean,
    val windowChrome: Boolean,
    val tray: Boolean,
    val autoUpdate: Boolean,
    val privilegeElevation: Boolean,
    val moddedServers: Boolean,
    val serverLoaders: List<String>,
    val minigames: Boolean,
    val fileImport: Boolean,
    val multipleRunningServers: Boolean,
    val backgroundExecution: Boolean,
    val backups: Boolean,
    val deviceWebsocket: Boolean,
    val haptics: Boolean,
    val nativeShare: Boolean,
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
            // True, and independent of the line above: a phone will never run a
            // Java client, but knowing *who the player is* has its own uses
            // with no client anywhere near them. Minigame stats are keyed on a
            // Minecraft uuid, which is the case that forced this key to exist —
            // without it a phone's Minigames Hub could only ever show a
            // signed-in user zero of their own numbers.
            //
            // [MinecraftAuth] answers all three `minecraft:auth:*` invokes and
            // emits both events. The flow is OAuth device code rather than a
            // redirect, because the public Xbox client the desktop uses accepts
            // only a Microsoft-hosted redirect that a phone cannot intercept.
            //
            // Note this is not the only way a phone learns its account, and on
            // most phones it will not be the one that fires: the API reports an
            // account already linked from the desktop app, which needs no
            // sign-in at all. This flag is about whether the *host* can run
            // one, which is a different question and the one the UI gates the
            // sign-in button on.
            minecraftAccount = true,
            windowChrome = false,
            tray = false,
            // True, and it means something different than on desktop: this app
            // replaces its *UI bundle* over the air rather than its binary, and
            // "install" rebuilds the WebView rather than restarting anything.
            // Same three channels, same modal, no platform branch in the UI.
            autoUpdate = true,
            privilegeElevation = false,
            // The JVM backend runs Bukkit-family plugins and Forge/Fabric mods.
            moddedServers = true,
            // Which loaders, specifically — `moddedServers` alone was too blunt
            // and the create flow paid for it: it offered all six loaders the
            // desktop offers, including Spigot, which `Loader::parse` refuses on
            // a phone because Spigot is *compiled* on the device by BuildTools
            // and the staged runtime has no `javac`. A player could pick it,
            // configure a world, and only find out at launch.
            //
            // Not derived from `Core.hostableLoaders()` at runtime, even though
            // that exists and is the same answer: this constant is injected at
            // document start, before the UI can await anything, and it is what
            // `scripts/check-capabilities.js` diffs against the contract. The
            // drift that matters — this list against the code that refuses — is
            // caught in Rust instead, by
            // `the_android_contract_advertises_exactly_the_loaders_this_core_hosts`.
            serverLoaders = listOf("vanilla", "paper", "fabric", "quilt", "neoforge", "forge"),
            // The JVM backend runs Paper, which is what minigames ship as
            // plugins for, so a phone can host one.
            //
            // It could not until the two halves that deliver a game existed.
            // `CUSTOM_PLUGINS` is how our own jars — the framework, the
            // BedWars fork — reach `plugins/`, and nothing on this host
            // fetched them ([PluginInstaller] now does); and a server's
            // settings are written to files rather than exported, so a plugin
            // calling `System.getenv` saw none of the ones it reads
            // ([Core.pluginEnv] now forwards our namespace). Flipping this
            // flag alone would have offered a Host button that produced a bare
            // Paper world with no game in it — which is the failure this whole
            // capability layer exists to make impossible, arriving through the
            // one route it does not check.
            //
            // This gates *hosting* only. The hub itself — browsing games, live
            // servers, leaderboards — is ungated on every host and always was.
            minigames = true,
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
            // restic, and it ships for both ABIs now. Deliberately not derived
            // from `ResticEngine.isAvailable`: this constant describes the
            // platform, and a build that forgot to stage the binary should be
            // fixed rather than quietly advertising less. `verifyNativePayload`
            // in `app/build.gradle.kts` is what enforces that — it fails a
            // release whose `jniLibs` is missing `librestic.so`, because the
            // alternative is an app that offers backups and silently does
            // nothing, and a world played here that never becomes the newest
            // snapshot, so the next desktop launch restores over it.
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
            // The page says what the user just did and `Haptics` decides what
            // it feels like. True because the platform can, not because every
            // device will: `performHapticFeedback` is a no-op when the owner
            // has touch feedback switched off, and that is the setting doing
            // its job rather than something for this flag to second-guess.
            haptics = true,
            // False because this host does not implement `share-content`, not
            // because the platform cannot — `Intent.createChooser` over
            // ACTION_SEND is exactly what this would be. Every share surface
            // in the UI falls back to the clipboard when this is false, which
            // is what a lobby invite does today.
            //
            // Reporting the truth is the whole job of this constant. A `true`
            // here would produce a Share row that reaches a channel nothing
            // answers, which is worse than a Copy row that works.
            nativeShare = false,
        )
    }
}
