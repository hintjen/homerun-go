/**
 * The build targets, and where each one's artifact has to land.
 *
 * One table so the build scripts, the doctor, and the docs cannot disagree
 * about triples or output paths.
 */
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const CRATE = path.join(ROOT, "rust", "homerun-pumpkin-ffi");
const CRATE_NAME = "homerun_pumpkin_ffi";

/**
 * The JVM launcher. A separate crate because it is an executable, not a
 * library, and it ships renamed — see `outName`.
 */
const LAUNCHER_CRATE = path.join(ROOT, "rust", "homerun-java-launcher");

/**
 * Pumpkin as a child process. A separate crate for the same reason as the
 * launcher — it is an executable — and it ships renamed for the same two
 * rules. Android links no Pumpkin at all now: the engine is this binary, so
 * `pumpkin-engine` is absent from the two Android feature lists below and the
 * `.so` they produce is ~7 MB rather than ~80 MB.
 */
const PUMPKIN_CRATE = path.join(ROOT, "rust", "homerun-pumpkin-bin");

/**
 * `homerun-core` over Node-API, for Homerun Desktop.
 *
 * The odd one out in this table: it is not part of either app here, and
 * nothing in this repo loads it. It is built here because this is where the
 * core lives, and shipped to the desktop as a downloaded artifact — the same
 * arrangement restic and wireproxy already have there — so that adopting the
 * core costs that build a *file* rather than a Rust toolchain.
 * `docs/shared-core.md` names the cost being avoided: "Adding Rust to the
 * desktop build is real CI work and a new way for a release to fail."
 */
const CORE_NODE_CRATE = path.join(ROOT, "rust", "homerun-core-node");

/**
 * The Android API level the native code is linked against. The iOS
 * equivalent is `deploymentTarget` on the two targets below; this one is
 * shared, because every Android target ships in the same app and that app
 * has one `minSdk`.
 *
 * **Keep this equal to `minSdk` in `android/app/build.gradle.kts`.**
 * cargo-ndk defaults to 21 when it is not told, and 21 is five years below
 * what the app claims to support — so the NDK hands the linker the API 21
 * stubs and every symbol added since is simply absent. That is survivable
 * for the `.so`, which is allowed to carry undefined symbols and resolve
 * them against the real libc at load time, and fatal for the two
 * executables, which must resolve everything at link.
 *
 * It stayed invisible for as long as nothing reached past API 21. The
 * Pumpkin pin bump to 09f1d4df pulled in webrtc, which calls `getifaddrs`
 * — added to Bionic in API 24 — and `homerun-pumpkin-bin` stopped linking.
 */
const ANDROID_API_LEVEL = 26;

/**
 * `kind` decides how it is built:
 *   cargo — plain `cargo build --target <triple>`
 *   ndk   — `cargo ndk -t <abi> build`, which supplies the NDK toolchain
 *   host  — the machine's own target, for tests
 *
 * `features` is the cargo feature list, per target rather than per script,
 * because the two platforms no longer want the same one. `backup-engine`
 * links a restic-compatible library and costs ~5.6 MB; iOS needs it because
 * it cannot spawn a process, and Android must not have it because it ships
 * the restic binary instead. `device-ws` is on for both phones and off for
 * the host build, which is what keeps `npm test` free of a TLS stack — note
 * that it pulls in `aws-lc-sys`, so an iOS build needs `cmake` on the machine
 * doing it. `--stub` overrides all of this with nothing, which is how you
 * check the FFI surface for a target in seconds.
 */
const TARGETS = {
  ios: {
    label: "iOS device",
    kind: "cargo",
    triple: "aarch64-apple-ios",
    deploymentTarget: "16.0",
    features: ["pumpkin-engine", "backup-engine", "device-ws"],
    artifact: `lib${CRATE_NAME}.a`,
    outDir: path.join(ROOT, "ios", "HomerunHost", "lib"),
    requiresMacOS: true,
  },
  "ios-sim": {
    label: "iOS simulator (Apple silicon)",
    kind: "cargo",
    triple: "aarch64-apple-ios-sim",
    deploymentTarget: "16.0",
    features: ["pumpkin-engine", "backup-engine", "device-ws"],
    artifact: `lib${CRATE_NAME}.a`,
    outDir: path.join(ROOT, "ios", "HomerunHost", "lib", "sim"),
    requiresMacOS: true,
  },
  android: {
    label: "Android arm64 (devices)",
    kind: "ndk",
    triple: "aarch64-linux-android",
    abi: "arm64-v8a",
    artifact: `lib${CRATE_NAME}.so`,
    features: ["process-engine", "device-ws"],
    // jniLibs is the only place Android will exec/load from on API 29+.
    outDir: path.join(
      ROOT, "android", "app", "src", "main", "jniLibs", "arm64-v8a"
    ),
  },
  "android-x86_64": {
    label: "Android x86_64 (emulator)",
    kind: "ndk",
    triple: "x86_64-linux-android",
    abi: "x86_64",
    artifact: `lib${CRATE_NAME}.so`,
    features: ["process-engine", "device-ws"],
    outDir: path.join(
      ROOT, "android", "app", "src", "main", "jniLibs", "x86_64"
    ),
  },
  // The JVM launcher, staged as `libjavabin.so`: Android only packages
  // jniLibs entries matching `lib*.so`, and only files under
  // nativeLibraryDir may be exec'd. Both rules, one rename.
  "java-launcher": {
    label: "Java launcher, Android arm64 (devices)",
    kind: "ndk",
    crate: LAUNCHER_CRATE,
    triple: "aarch64-linux-android",
    abi: "arm64-v8a",
    artifact: "homerun-java-launcher",
    outName: "libjavabin.so",
    outDir: path.join(
      ROOT, "android", "app", "src", "main", "jniLibs", "arm64-v8a"
    ),
  },
  "java-launcher-x86_64": {
    label: "Java launcher, Android x86_64 (emulator)",
    kind: "ndk",
    crate: LAUNCHER_CRATE,
    triple: "x86_64-linux-android",
    abi: "x86_64",
    artifact: "homerun-java-launcher",
    outName: "libjavabin.so",
    outDir: path.join(
      ROOT, "android", "app", "src", "main", "jniLibs", "x86_64"
    ),
  },
  // Pumpkin, staged as `libpumpkin.so`. Same two rules as the launcher above:
  // Android packages only `lib*.so` from jniLibs, and only files under
  // nativeLibraryDir may be exec'd on API 29+.
  "pumpkin-bin": {
    label: "Pumpkin server, Android arm64 (devices)",
    kind: "ndk",
    crate: PUMPKIN_CRATE,
    triple: "aarch64-linux-android",
    abi: "arm64-v8a",
    artifact: "homerun-pumpkin",
    outName: "libpumpkin.so",
    outDir: path.join(
      ROOT, "android", "app", "src", "main", "jniLibs", "arm64-v8a"
    ),
  },
  "pumpkin-bin-x86_64": {
    label: "Pumpkin server, Android x86_64 (emulator)",
    kind: "ndk",
    crate: PUMPKIN_CRATE,
    triple: "x86_64-linux-android",
    abi: "x86_64",
    artifact: "homerun-pumpkin",
    outName: "libpumpkin.so",
    outDir: path.join(
      ROOT, "android", "app", "src", "main", "jniLibs", "x86_64"
    ),
  },
  // Windows x64 only, because that is the only architecture Homerun Desktop
  // ships. A `.node` is a plain shared library with a renamed extension;
  // Node-API is ABI-stable across Node *and* Electron versions, so this needs
  // no rebuild when either moves — which is the whole reason it is Node-API
  // and not a raw V8 addon.
  "core-node": {
    label: "homerun-core for Homerun Desktop (Node addon, Windows x64)",
    kind: "cargo",
    crate: CORE_NODE_CRATE,
    triple: "x86_64-pc-windows-msvc",
    artifact: "homerun_core_node.dll",
    outName: "homerun_core.node",
    outDir: path.join(ROOT, "dist", "desktop"),
    requiresWindows: true,
  },
  host: {
    label: "this machine (tests only)",
    kind: "host",
    artifact: null,
    outDir: null,
  },
};

/** Targets a platform needs before its app can be built. */
const PLATFORM_TARGETS = {
  ios: ["ios", "ios-sim"],
  android: [
    "android", "android-x86_64",
    "java-launcher", "java-launcher-x86_64",
    "pumpkin-bin", "pumpkin-bin-x86_64",
  ],
};

/** Where each host expects the UI bundle staged. */
const UI_DESTINATIONS = {
  ios: path.join(ROOT, "ios", "HomerunHost", "web"),
  android: path.join(ROOT, "android", "app", "src", "main", "assets", "web"),
};

module.exports = { ROOT, CRATE, CRATE_NAME, LAUNCHER_CRATE, PUMPKIN_CRATE, ANDROID_API_LEVEL, TARGETS, PLATFORM_TARGETS, UI_DESTINATIONS };
