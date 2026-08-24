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
 * Flags, after the command:
 *
 *   --api <url>    build against another backend — the staging API, a laptop.
 *                  Shorthand for `-PapiUrl=<url>`, because that is the only
 *                  property anybody passes and `--api` is what people try.
 *   --fresh        wipe the app's data after installing, so it launches as a
 *                  genuine first run. **Usually wanted with `--api`.**
 *   --no-ota       ignore over-the-air UI bundles: this build serves the UI it
 *                  was built with and nothing else. Shorthand for
 *                  `-PotaUpdates=off`. For iterating on the shared UI, where an
 *                  update landing mid-session would replace what you staged.
 *   -P<name>=<v>   any other gradle property, forwarded untouched.
 *
 * ## Why `--api` alone is usually not enough
 *
 * `-PapiUrl` sets `BuildConfig.API_URL`, which the *host* reads. The page reads
 * its own `localStorage.apiUrl`, seeded from the host's value **only on first
 * run** (`pages/index.tsx`) so a hand-picked backend survives every remount.
 * Sensible there, and it means rebuilding with a different `--api` changes
 * nothing about where the page sends anything — which is most requests,
 * registration and login included. The build succeeds and the app keeps talking
 * to the old backend, which is a genuinely difficult thing to notice.
 *
 * So `--api` reads back what the device actually has stored and says so when the
 * two disagree. `--fresh` is the fix, and it is needed **once**, when switching
 * a device from one backend to another:
 *
 *   npm run android:run:staging:fresh    switching to staging, or starting over
 *   npm run android:run:staging          every run after that
 *
 * Clearing on every launch would make the target useless for the thing it is
 * for — iterating against staging while signed in — so the plain target keeps
 * the data and the warning is what tells you when you need the other one.
 *
 *   HOMERUN_JAVA_HOME   a JDK to use, ahead of JAVA_HOME
 *   HOMERUN_AVD         which AVD `emulator` starts (default: homerun_api36)
 *   ANDROID_SERIAL      which device adb targets, when several are attached
 *   HOMERUN_SKIP_NATIVE do not refresh jniLibs (Kotlin-only iteration)
 *   HOMERUN_API_URL     default for `--api`
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
// API 36, and it has to keep pace with `targetSdk` in app/build.gradle.kts.
// Every behaviour change a target bump opts into is gated on the *device's*
// version as well as the app's, so an API 35 emulator shows none of them --
// the app looks correct here right up until it reaches a phone where it is
// not. Anyone still holding the old homerun_api35 device can set HOMERUN_AVD.
const DEFAULT_AVD = process.env.HOMERUN_AVD || "homerun_api36";

const die = (message) => {
  console.error(`\n${message}\n`);
  process.exit(1);
};

// --- flags -----------------------------------------------------------------

/**
 * Everything after the command: `--api`, `--fresh`, and any `-P` to forward.
 *
 * Hand-rolled rather than a dependency, and permissive by design about
 * `--api=x` versus `--api x` — both are what people type, and refusing one of
 * them teaches nothing.
 */
function parseFlags(argv) {
  const flags = { apiUrl: process.env.HOMERUN_API_URL || null, fresh: false, props: [] };

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--fresh") {
      flags.fresh = true;
    } else if (arg === "--api") {
      flags.apiUrl = argv[++i];
      if (!flags.apiUrl) die("--api needs a URL, e.g. --api https://api.fractalnetworks.co");
    } else if (arg.startsWith("--api=")) {
      flags.apiUrl = arg.slice("--api=".length);
    } else if (arg === "--no-ota") {
      // Not a boolean on `flags`: it is one gradle property and nothing here
      // needs to know about it afterwards.
      flags.props.push("-PotaUpdates=off");
    } else if (arg.startsWith("-P")) {
      flags.props.push(arg);
    } else {
      die(
        `Unknown option "${arg}".\n` +
          "Expected --api <url>, --fresh, --no-ota, or a -P<name>=<value> gradle property."
      );
    }
  }

  if (flags.apiUrl) {
    let parsed;
    try {
      parsed = new URL(flags.apiUrl);
    } catch {
      die(`--api needs an absolute URL with a scheme and host, not "${flags.apiUrl}".`);
    }
    // The same shape `set-api-url` insists on, so a value that builds here
    // cannot be one the host would refuse at runtime. Warned rather than
    // refused for http: a laptop backend is a legitimate thing to point at, and
    // the host reads `BuildConfig.API_URL` without going through that check.
    if (parsed.protocol !== "https:") {
      console.warn(
        `\nWarning: ${flags.apiUrl} is not https. The host will use it, but the\n` +
          "page cannot set it at runtime — `set-api-url` refuses anything else.\n"
      );
    }
    flags.props.push(`-PapiUrl=${flags.apiUrl}`);
  }

  return flags;
}

/**
 * The API base the app on the device has actually stored, or null.
 *
 * Plaintext on purpose — `SecretStore` encrypts the two bearer tokens and
 * deliberately not this, so it can be read exactly like this when something is
 * pointed at the wrong backend.
 */
function deviceApiUrl() {
  try {
    const prefs = adb(
      ["shell", "run-as", APPLICATION_ID, "cat", "shared_prefs/homerun.xml"],
      { stdio: ["ignore", "pipe", "ignore"] }
    );
    return prefs.match(/name="api-url">([^<]+)</)?.[1] ?? null;
  } catch {
    // No app installed yet, no prefs file, or a release build that `run-as`
    // cannot read. None of those is worth failing a build over.
    return null;
  }
}

/**
 * Say where this build points, and that the page may not agree.
 *
 * Printed unconditionally rather than only on a detected mismatch, because the
 * thing that decides is the page's `localStorage`, and that is not readable from
 * here — it lives in the WebView's LevelDB. [deviceApiUrl] sees only the host's
 * copy, which the host stores *only* when the page's value differs from
 * `BuildConfig`, so it goes from absent to present a run later than would be
 * useful. Rather than dress that up as detection, this states the rule every
 * time and escalates when there is something concrete to show.
 */
function announceApiUrl(apiUrl) {
  const stored = deviceApiUrl();

  if (stored && stored !== apiUrl) {
    console.warn(
      `\n  Built for ${apiUrl}\n` +
        `  The app has  ${stored}  stored, and that wins: the page seeds its\n` +
        "  API URL on first run only, so its requests — registration and login\n" +
        "  included — still go to the old backend.\n\n" +
        "  Re-run with --fresh to wipe the data and let it re-seed.\n"
    );
    return;
  }

  console.log(
    `\n  Built for ${apiUrl}\n` +
      "  The page keeps its own copy, seeded on first run. If this device has\n" +
      "  run against a different backend, add --fresh or it will keep using it.\n"
  );
}

/** Wipe the app's data, so the next launch is a genuine first run. */
function clearAppData() {
  console.log("Clearing app data (worlds, credentials, staged runtimes) ...");
  adb(["shell", "pm", "clear", APPLICATION_ID], { stdio: "inherit" });
}

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
 * The ABI of the one attached device, or null when there is no single answer.
 *
 * Two callers need this and they must agree: [abisToBuild], to build only the
 * native library that device can load, and [gradle], which passes it on as
 * `-Pabi` so `verifyJavaRuntime` knows which architecture the staged Java
 * runtime has to match.
 *
 * Cached because it is a round trip to the device and the answer cannot change
 * within one invocation.
 */
let cachedDeviceAbi;
function deviceAbi() {
  if (cachedDeviceAbi === undefined) {
    try {
      const abi = adb(["shell", "getprop", "ro.product.cpu.abi"]).trim();
      cachedDeviceAbi = ABI_TARGETS[abi] ? abi : null;
    } catch (_) {
      // No device attached, or more than one.
      cachedDeviceAbi = null;
    }
  }
  return cachedDeviceAbi;
}

/**
 * Which ABIs to refresh before gradle packages them.
 *
 * A connected device narrows it to the one that will actually run, which is
 * what keeps the emulator loop fast — otherwise a change to the core would
 * rebuild arm64 too, for minutes, to produce a library nothing here can load.
 */
function abisToBuild() {
  const only = deviceAbi();
  if (only) return [only];

  const staged = Object.keys(ABI_TARGETS).filter((abi) =>
    fs.existsSync(path.join(JNI_LIBS, abi))
  );
  // No device attached. Refresh what is already staged.
  return staged.length ? staged : Object.keys(ABI_TARGETS);
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

  // Tell gradle which architecture this build is for, so `verifyJavaRuntime`
  // can refuse a Java runtime staged for the other one.
  //
  // This is not a nicety. The JRE lives in `assets/`, where the ABI filter
  // never looks, and it is staged one architecture at a time — so an APK built
  // after `npm run jre:android-x86_64` installs on a phone, launches, shows
  // every screen, and then cannot start a single server: the only `libjvm.so`
  // in it is for the wrong CPU, and you find out from a `dlopen` failure deep
  // in a server log. Without `-Pabi` that check is skipped entirely, which
  // made it useless in exactly the loop that switches between an emulator and
  // a phone. Found by installing on a real device.
  const abi = deviceAbi();
  const args = [...tasks, ...(abi ? [`-Pabi=${abi}`] : []), ...flags.props];

  const wrapper = path.join(ANDROID_DIR, process.platform === "win32" ? "gradlew.bat" : "gradlew");
  const jdk = resolveJdk();
  console.log(`JDK:  ${jdk}\nTask: ${args.join(" ")}\n`);

  const result = spawnSync(wrapper, args, {
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
        `  sdkmanager "system-images;android-36;google_apis;x86_64"\n` +
        `  avdmanager create avd -n ${DEFAULT_AVD} -k "system-images;android-36;google_apis;x86_64" -d pixel_7`
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
const flags = parseFlags(process.argv.slice(3));

/**
 * Install, then reset the data if asked, then say so if the API URL will not
 * take effect.
 *
 * `--fresh` runs *after* the install because installing over an existing app
 * keeps its data; clearing first would only wipe the old build's. The warning
 * runs last so it is the final thing on screen rather than scrolled away by
 * gradle, and it is skipped when `--fresh` has already made it moot.
 */
function installAndPrepare() {
  install();
  if (flags.fresh) clearAppData();
  else if (flags.apiUrl) announceApiUrl(flags.apiUrl);
}

switch (command) {
  case "build":
    gradle(["assembleDebug"]);
    break;
  case "install":
    gradle(["assembleDebug"]);
    installAndPrepare();
    break;
  case "run":
    gradle(["assembleDebug"]);
    installAndPrepare();
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
