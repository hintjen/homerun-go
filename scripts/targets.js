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
 * `kind` decides how it is built:
 *   cargo — plain `cargo build --target <triple>`
 *   ndk   — `cargo ndk -t <abi> build`, which supplies the NDK toolchain
 *   host  — the machine's own target, for tests
 *
 * `features` is the cargo feature list, per target rather than per script,
 * because the two platforms no longer want the same one. `backup-engine`
 * links a restic-compatible library and costs ~5.6 MB; iOS needs it because
 * it cannot spawn a process, and Android must not have it because it ships
 * the restic binary instead. `--stub` overrides all of this with nothing,
 * which is how you check the FFI surface for a target in seconds.
 */
const TARGETS = {
  ios: {
    label: "iOS device",
    kind: "cargo",
    triple: "aarch64-apple-ios",
    deploymentTarget: "16.0",
    features: ["pumpkin-engine", "backup-engine"],
    artifact: `lib${CRATE_NAME}.a`,
    outDir: path.join(ROOT, "ios", "HomerunHost", "lib"),
    requiresMacOS: true,
  },
  "ios-sim": {
    label: "iOS simulator (Apple silicon)",
    kind: "cargo",
    triple: "aarch64-apple-ios-sim",
    deploymentTarget: "16.0",
    features: ["pumpkin-engine", "backup-engine"],
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
    features: ["pumpkin-engine", "process-engine"],
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
    features: ["pumpkin-engine", "process-engine"],
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
  android: ["android", "android-x86_64", "java-launcher", "java-launcher-x86_64"],
};

/** Where each host expects the UI bundle staged. */
const UI_DESTINATIONS = {
  ios: path.join(ROOT, "ios", "HomerunHost", "web"),
  android: path.join(ROOT, "android", "app", "src", "main", "assets", "web"),
};

module.exports = { ROOT, CRATE, CRATE_NAME, LAUNCHER_CRATE, TARGETS, PLATFORM_TARGETS, UI_DESTINATIONS };
