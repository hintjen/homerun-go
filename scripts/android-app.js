#!/usr/bin/env node
/**
 * Builds, installs, and launches the Android host.
 *
 *   node scripts/android-app.js build      assemble the debug APK
 *   node scripts/android-app.js install    build, then install on the device
 *   node scripts/android-app.js run        install, then launch and follow logs
 *   node scripts/android-app.js emulator   start the AVD and wait for boot
 *   node scripts/android-app.js logs       follow the app's logcat
 *
 * Gradle does the real work; this exists for the three things it cannot do
 * for itself — pick a JDK it can actually run on, make sure the shared UI
 * bundle has been staged, and rebuild the Rust in `jniLibs`, which gradle
 * treats as a checked-in binary and will happily package stale.
 *
 *   HOMERUN_JAVA_HOME   a JDK to use, ahead of JAVA_HOME
 *   HOMERUN_AVD         which AVD `emulator` starts (default: homerun_api35)
 *   ANDROID_SERIAL      which device adb targets, when several are attached
 *   HOMERUN_SKIP_NATIVE do not refresh jniLibs (Kotlin-only iteration)
 */
const { execFileSync, spawn, spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const { ROOT, UI_DESTINATIONS } = require("./targets");
const { findJdk, INSTALL_HINT } = require("./jdk");

const ANDROID_DIR = path.join(ROOT, "android");
// Debug builds carry an applicationIdSuffix so a debug and a release build
// can coexist on one device. The activity class name is not suffixed.
const APPLICATION_ID = "app.gethomerun.mobile.debug";
const LAUNCH_ACTIVITY = `${APPLICATION_ID}/app.gethomerun.mobile.MainActivity`;
const DEFAULT_AVD = process.env.HOMERUN_AVD || "homerun_api35";

const die = (message) => {
  console.error(`\n${message}\n`);
  process.exit(1);
};

// --- SDK -------------------------------------------------------------------

function sdkRoot() {
  const root = process.env.ANDROID_HOME || process.env.ANDROID_SDK_ROOT;
  if (!root || !fs.existsSync(root)) {
    die(
      "No Android SDK.\n" +
        "Set ANDROID_HOME (and ANDROID_SDK_ROOT) to your SDK directory —\n" +
        "typically %LOCALAPPDATA%\\Android\\Sdk or ~/Library/Android/sdk."
    );
  }
  return root;
}

const exe = (name) => (process.platform === "win32" ? `${name}.exe` : name);

function sdkTool(...segments) {
  const tool = path.join(sdkRoot(), ...segments.slice(0, -1), exe(segments.at(-1)));
  if (!fs.existsSync(tool)) {
    die(
      `Missing ${segments.join("/")} in the SDK.\n` +
        `Install it:  sdkmanager "${segments[0] === "emulator" ? "emulator" : "platform-tools"}"`
    );
  }
  return tool;
}

const adbPath = () => sdkTool("platform-tools", "adb");

function adb(args, opts = {}) {
  const serial = process.env.ANDROID_SERIAL;
  const prefixed = serial ? ["-s", serial, ...args] : args;
  return execFileSync(adbPath(), prefixed, { encoding: "utf8", ...opts });
}

// --- JDK -------------------------------------------------------------------

function resolveJdk() {
  const { jdk, seen } = findJdk();
  if (jdk) return jdk.home;
  die(
    "No usable JDK found.\n" +
      (seen.length
        ? `Saw: ${seen.map((s) => `${s.major} at ${s.home}`).join(", ")}\n\n`
        : "\n") +
      INSTALL_HINT
  );
}

// --- native libraries ------------------------------------------------------

const JNI_LIBS = path.join(ANDROID_DIR, "app", "src", "main", "jniLibs");

/** ABI directory -> the `build-rust.js` targets that fill it. */
const ABI_TARGETS = {
  "arm64-v8a": ["android", "java-launcher"],
  x86_64: ["android-x86_64", "java-launcher-x86_64"],
};

/**
 * Which ABIs to refresh before gradle packages them.
 *
 * A connected device narrows it to the one that will actually run, which is
 * what keeps the emulator loop fast — otherwise a change to the core would
 * rebuild arm64 too, for minutes, to produce a library nothing here can load.
 */
function abisToBuild() {
  const staged = Object.keys(ABI_TARGETS).filter((abi) =>
    fs.existsSync(path.join(JNI_LIBS, abi))
  );
  const fallback = staged.length ? staged : Object.keys(ABI_TARGETS);

  try {
    const abi = adb(["shell", "getprop", "ro.product.cpu.abi"]).trim();
    if (ABI_TARGETS[abi]) return [abi];
  } catch (_) {
    // No device attached, or more than one. Refresh what is already staged.
  }
  return fallback;
}

/**
 * Rebuild the Rust the APK is about to package.
 *
 * **Not a convenience.** Gradle has no idea `jniLibs` is generated, so it will
 * happily package a library built before your last change to
 * `core_dispatch.rs` — and the app then fails at run time with `the native
 * core has no method "…"`, which reads like a bug in Kotlin that has just
 * compiled cleanly. That cost real time once. It should not cost it twice.
 *
 * Cargo makes the unchanged case about a second and a half, so there is
 * nothing to save by skipping it. `HOMERUN_SKIP_NATIVE=1` is there for
 * iterating on Kotlin alone, when you know the libraries are current.
 */
function stageNative() {
  if (process.env.HOMERUN_SKIP_NATIVE) {
    console.log("Native: skipped — HOMERUN_SKIP_NATIVE is set\n");
    return;
  }

  const abis = abisToBuild();
  console.log(`Native: refreshing ${abis.join(", ")}`);

  for (const abi of abis) {
    for (const target of ABI_TARGETS[abi]) {
      const result = spawnSync(
        process.execPath,
        [path.join(__dirname, "build-rust.js"), target],
        { cwd: ROOT, stdio: "inherit" }
      );
      if (result.status !== 0) {
        die(
          `Could not build the native library for "${target}".\n\n` +
            "Stopping here on purpose: the APK would otherwise package whatever\n" +
            "was staged before, and the app would fail at run time with\n" +
            '`the native core has no method "…"` — a long way from this cause.\n\n' +
            "Fix the build, or set HOMERUN_SKIP_NATIVE=1 if you are certain the\n" +
            "staged libraries are already current."
        );
      }
    }
  }
}

// --- Gradle ----------------------------------------------------------------

function gradle(tasks) {
  const bundle = path.join(UI_DESTINATIONS.android, "index.html");
  if (!fs.existsSync(bundle)) {
    die(
      "The shared UI bundle is not staged, so the app would show a blank screen.\n" +
        "Run:  npm run ui:android"
    );
  }

  stageNative();

  const wrapper = path.join(ANDROID_DIR, process.platform === "win32" ? "gradlew.bat" : "gradlew");
  const jdk = resolveJdk();
  console.log(`JDK:  ${jdk}\nTask: ${tasks.join(" ")}\n`);

  const result = spawnSync(wrapper, tasks, {
    cwd: ANDROID_DIR,
    stdio: "inherit",
    env: { ...process.env, JAVA_HOME: jdk },
    shell: process.platform === "win32",
  });
  if (result.status !== 0) process.exit(result.status ?? 1);
}

// --- device ----------------------------------------------------------------

function attachedDevices() {
  const out = execFileSync(adbPath(), ["devices"], { encoding: "utf8" });
  return out
    .split(/\r?\n/)
    .slice(1)
    .map((line) => line.trim().split(/\s+/))
    .filter(([serial, state]) => serial && state === "device")
    .map(([serial]) => serial);
}

function requireDevice() {
  const devices = attachedDevices();
  if (!devices.length) {
    die(
      "No device or emulator is attached.\n" +
        "Start one:  npm run android:emulator\n" +
        "or plug in a phone with USB debugging enabled."
    );
  }
  if (devices.length > 1 && !process.env.ANDROID_SERIAL) {
    die(
      `Several devices attached: ${devices.join(", ")}\n` +
        "Pick one with ANDROID_SERIAL=<serial>."
    );
  }
  return process.env.ANDROID_SERIAL || devices[0];
}

function startEmulator() {
  const emulator = sdkTool("emulator", "emulator");
  const avds = execFileSync(emulator, ["-list-avds"], { encoding: "utf8" })
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter(Boolean);

  if (!avds.includes(DEFAULT_AVD)) {
    die(
      `No AVD named "${DEFAULT_AVD}".${avds.length ? ` Have: ${avds.join(", ")}` : ""}\n\n` +
        "Create one:\n" +
        `  sdkmanager "system-images;android-35;google_apis;x86_64"\n` +
        `  avdmanager create avd -n ${DEFAULT_AVD} -k "system-images;android-35;google_apis;x86_64" -d pixel_7`
    );
  }

  console.log(`Starting ${DEFAULT_AVD} ...`);
  // Detached: the emulator outlives this script, which is the point.
  spawn(emulator, ["-avd", DEFAULT_AVD, "-no-boot-anim"], {
    detached: true,
    stdio: "ignore",
  }).unref();

  execFileSync(adbPath(), ["wait-for-device"], { stdio: "inherit" });
  for (let i = 0; i < 150; i++) {
    const booted = spawnSync(adbPath(), ["shell", "getprop", "sys.boot_completed"], {
      encoding: "utf8",
    }).stdout?.trim();
    if (booted === "1") {
      console.log("Booted.");
      return;
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 2000);
  }
  die("The emulator started but never reported sys.boot_completed.");
}

function apkPath() {
  const apk = path.join(
    ANDROID_DIR, "app", "build", "outputs", "apk", "debug", "app-debug.apk"
  );
  if (!fs.existsSync(apk)) die(`No APK at ${apk}. Run the build first.`);
  return apk;
}

function install() {
  const serial = requireDevice();
  console.log(`Installing on ${serial} ...`);
  adb(["install", "-r", "-t", apkPath()], { stdio: "inherit" });
}

function launch() {
  adb(["shell", "am", "force-stop", APPLICATION_ID], { stdio: "ignore" });
  adb(["shell", "am", "start", "-n", LAUNCH_ACTIVITY], { stdio: "inherit" });
}

/**
 * Every `Homerun*` logcat tag the host declares, read from the source.
 *
 * Hardcoding three of them cost real debugging time: `logcat`'s `*:S` silences
 * everything not named, so the tunnel (HomerunTunnel), the backups
 * (HomerunBackup) and the server itself (HomerunJava) were invisible while a
 * tunnel bug was being chased through this very command. A list derived from
 * the code cannot drift from it — a new `TAG = "HomerunFoo"` is followed the
 * moment it exists.
 */
function hostLogTags() {
  const dir = path.join(ANDROID_DIR, "app", "src", "main", "java", "app", "gethomerun", "mobile");
  const tags = new Set();
  const walk = (at) => {
    for (const entry of fs.readdirSync(at, { withFileTypes: true })) {
      const full = path.join(at, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith(".kt")) {
        // `TAG\w*` deliberately: MainActivity declares a second one as
        // TAG_WEB, and that is the WebView console — the last tag you want to
        // drop.
        for (const m of fs.readFileSync(full, "utf8").matchAll(/TAG\w*\s*=\s*"(Homerun\w*)"/g)) {
          tags.add(m[1]);
        }
      }
    }
  };
  try {
    walk(dir);
  } catch {
    // Running from a packaged checkout without sources: fall back rather than
    // leaving the user with no logs at all.
  }
  if (tags.size === 0) ["HomerunHost", "HomerunBridge", "HomerunWeb"].forEach((t) => tags.add(t));
  return [...tags].sort();
}

function logs() {
  requireDevice();
  adb(["logcat", "-c"], { stdio: "ignore" });
  const tags = hostLogTags();
  console.log(`Following ${tags.join(" / ")} — Ctrl-C to stop.\n`);
  const serial = process.env.ANDROID_SERIAL;
  spawn(
    adbPath(),
    [
      ...(serial ? ["-s", serial] : []),
      "logcat",
      ...tags.map((t) => `${t}:V`),
      "chromium:E", "AndroidRuntime:E",
      "*:S",
    ],
    { stdio: "inherit" }
  );
}

// --- entry point -----------------------------------------------------------

const command = process.argv[2] || "run";

switch (command) {
  case "build":
    gradle(["assembleDebug"]);
    break;
  case "install":
    gradle(["assembleDebug"]);
    install();
    break;
  case "run":
    gradle(["assembleDebug"]);
    install();
    launch();
    logs();
    break;
  case "emulator":
    startEmulator();
    break;
  case "logs":
    logs();
    break;
  default:
    console.error(
      `Unknown command "${command}". Expected: build, install, run, emulator, logs`
    );
    process.exit(2);
}
