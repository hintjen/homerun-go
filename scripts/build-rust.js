#!/usr/bin/env node
/**
 * Builds the Pumpkin FFI for a mobile target and puts the artifact where
 * the host project expects it.
 *
 *   node scripts/build-rust.js ios
 *   node scripts/build-rust.js android --debug
 *   node scripts/build-rust.js host          # tests only, no artifact
 *
 * Every prerequisite is checked before cargo runs, because the native
 * failure modes are unhelpful: a missing target gives a linker error about
 * a file you never mentioned, and a missing NDK gives one about `cc`.
 */
const { execFileSync } = require("child_process");
const { featureProblems } = require("./check-features");
const fs = require("fs");
const path = require("path");

const { CRATE, TARGETS, ANDROID_API_LEVEL } = require("./targets");

const args = process.argv.slice(2);
const name = args.find((a) => !a.startsWith("-"));
const debug = args.includes("--debug");

if (!name || !TARGETS[name]) {
  console.error(
    `usage: build-rust.js <target> [--debug]\n\ntargets:\n` +
      Object.entries(TARGETS)
        .map(([k, t]) => `  ${k.padEnd(15)} ${t.label}`)
        .join("\n")
  );
  process.exit(2);
}

const target = TARGETS[name];
const profile = debug ? "debug" : "release";

function run(cmd, cmdArgs, opts = {}) {
  // shell:true — Node refuses to execFile .cmd shims on Windows (EINVAL)
  // since the 18.20.2 / 20.12.2 security change.
  return execFileSync(cmd, cmdArgs, {
    stdio: "inherit",
    shell: true,
    cwd: target.crate || CRATE,
    ...opts,
  });
}

function capture(cmd, cmdArgs) {
  try {
    return execFileSync(cmd, cmdArgs, {
      encoding: "utf8",
      shell: true,
      stdio: ["ignore", "pipe", "ignore"],
    });
  } catch {
    return null;
  }
}

function fail(message) {
  console.error(`\n${message}\n`);
  process.exit(1);
}

// --- prerequisites --------------------------------------------------------

if (!capture("cargo", ["--version"])) {
  fail(
    "cargo not found on PATH.\n" +
      "  Install Rust: https://rustup.rs\n" +
      "  Already installed? It lives in ~/.cargo/bin — add that to PATH."
  );
}

if (target.requiresWindows && process.platform !== "win32") {
  fail(
    `${target.label} can only be built on Windows (it links against the MSVC\n` +
      `  runtime the desktop app loads it into).\n` +
      `  You are on ${process.platform}.`
  );
}

if (target.requiresMacOS && process.platform !== "darwin") {
  fail(
    `${target.label} can only be built on macOS (it needs Xcode's linker and SDKs).\n` +
      `  You are on ${process.platform}. Build the Android targets here and\n` +
      "  leave iOS to a Mac or CI runner."
  );
}

if (target.kind === "cargo" || target.kind === "ndk") {
  const installed = capture("rustup", ["target", "list", "--installed"]) || "";
  if (!installed.split(/\r?\n/).includes(target.triple)) {
    fail(
      `Rust target ${target.triple} is not installed.\n` +
        `  rustup target add ${target.triple}`
    );
  }
}

if (target.kind === "ndk") {
  if (!capture("cargo", ["ndk", "--version"])) {
    fail(
      "cargo-ndk is not installed — it supplies the NDK toolchain to cargo.\n" +
        "  cargo install cargo-ndk\n" +
        "  You also need the Android NDK (Android Studio > SDK Manager > NDK)\n" +
        "  and ANDROID_NDK_HOME pointing at it."
    );
  }
  if (!process.env.ANDROID_NDK_HOME && !process.env.ANDROID_NDK_ROOT) {
    console.warn(
      "WARNING: neither ANDROID_NDK_HOME nor ANDROID_NDK_ROOT is set.\n" +
        "         cargo-ndk can sometimes find the NDK anyway; if the build\n" +
        "         fails on a missing linker, that is why.\n"
    );
  }
}

// --- where cargo will put it -----------------------------------------------

const crate = target.crate || CRATE;

/*
  Honor CARGO_TARGET_DIR the way cargo does (resolved against the cwd cargo ran
  in, which is the crate dir). CI sets it to a directory outside the workspace
  so `git clean` between runs cannot throw the build cache away.

  Then give each crate its own tree inside it. These crates are standalone —
  there is no workspace Cargo.toml — so a plain `cargo build` already keeps
  them apart in `<crate>/target`. A single shared CARGO_TARGET_DIR is what
  collapses them into one, and that collapse is not cosmetic.

  `homerun-pumpkin-ffi` is crate-type = ["staticlib", "cdylib", "rlib"], and
  `homerun-pumpkin-bin` depends on it with features = ["pumpkin-engine"]. So
  building the binary also builds that cdylib, with different features, into
  the same `<triple>/<profile>/libhomerun_pumpkin_ffi.so` the `android` target
  stages from.

  On 2026-08-31 that is what shipped. Three publish runs against one warm
  cache: the first staged 6.2 MB, the two after it staged 110.3 MB — the
  binary's copy, with no process-engine and no device-ws — while
  `rust:android` reported success in 3.05s having relinked nothing. 0.1.0
  (1013) reached Google Play, where every server launch failed with "This
  build cannot run a server as a separate process.", Java and Pumpkin alike,
  since Android spawns both as child processes.

  Which piece of cargo's bookkeeping let two builds land on one file is not
  established. A fresh unit *is* re-uplifted — overwrite that path by hand
  and the next build puts its own artifact back — so a merely stale uplift
  is not the answer. Not sharing the directory removes the question; the
  check further down is what catches it if the answer is something else again.
*/
const targetRoot = process.env.CARGO_TARGET_DIR
  ? path.join(
      path.resolve(crate, process.env.CARGO_TARGET_DIR),
      path.basename(crate)
    )
  : path.join(crate, "target");

// cargo has to agree with the path we read back from below.
const cargoEnv = { ...process.env, CARGO_TARGET_DIR: targetRoot };

// --- build ----------------------------------------------------------------

console.log(`\nBuilding ${target.label} (${profile})\n`);

const profileArgs = debug ? [] : ["--release"];

// The device builds link the real server; the host build deliberately does
// not, so `cargo test` stays a couple of seconds and needs no Pumpkin. Pass
// --stub to cross-compile without the engine, which is how you check that the
// FFI surface itself still builds for a target without waiting for wasmtime.
//
// Which features a target wants lives in targets.js, because iOS and Android
// no longer want the same ones — see `backup-engine` there.
const features = args.includes("--stub") ? [] : target.features ?? [];
const engineArgs = features.length ? ["--features", features.join(",")] : [];

if (target.kind === "host") {
  run("cargo", ["build", ...profileArgs], { env: cargoEnv });
  console.log("\nHost build done. `cargo test` runs the suite.");
  process.exit(0);
}

if (target.kind === "ndk") {
  // cargo-ndk wants its own flags before `build`.
  // --platform, or cargo-ndk links against its own default of API 21 and
  // anything Bionic gained since is an undefined symbol. See
  // ANDROID_API_LEVEL in targets.js for what that cost.
  run("cargo", [
    "ndk",
    "-t",
    target.abi,
    "--platform",
    String(ANDROID_API_LEVEL),
    "build",
    ...profileArgs,
    ...engineArgs,
  ], { env: cargoEnv });
} else {
  run(
    "cargo",
    ["build", ...profileArgs, ...engineArgs, "--target", target.triple],
    // cc-rs compiles the C in our native dependencies, and with nothing to
    // tell it otherwise it picks a deployment target old enough that
    // `___chkstk_darwin` does not exist — which surfaces as zstd failing to
    // link with undefined symbols and no mention of a deployment target
    // anywhere. Must match ios/project.yml, or the app links C built against
    // a different floor than the Swift beside it.
    target.deploymentTarget
      ? { env: { ...cargoEnv, IPHONEOS_DEPLOYMENT_TARGET: target.deploymentTarget } }
      : { env: cargoEnv }
  );
}

// --- stage the artifact ---------------------------------------------------

const built = path.join(targetRoot, target.triple, profile, target.artifact);
if (!fs.existsSync(built)) {
  fail(
    `Build reported success but ${target.artifact} is not at:\n  ${built}\n` +
      "  Check the crate-type in Cargo.toml (staticlib for iOS, cdylib for Android)."
  );
}

fs.mkdirSync(target.outDir, { recursive: true });
const dest = path.join(target.outDir, target.outName || target.artifact);
fs.copyFileSync(built, dest);

const mb = (fs.statSync(dest).size / 1024 / 1024).toFixed(1);
console.log(`\n${target.artifact} -> ${dest}  (${mb} MB)`);

// --- prove it is the library we asked for ---------------------------------

/*
  The per-crate target trees above should make a mismatched artifact
  impossible. This is what says so out loud if it happens anyway -- at the
  second it happens, rather than in a store listing.

  The markers and the reasoning live in check-features.js, which also runs
  standalone against any binary, including one pulled off a device.
*/
if (crate === CRATE) {
  const wrong = featureProblems(fs.readFileSync(dest), features);
  const asked = features.length ? features.join(", ") : "none (--stub)";
  if (wrong.length) {
    fail(
      `The staged ${target.artifact} is not the build that was asked for.\n` +
        wrong.map((w) => `  - ${w}`).join("\n") +
        `\n\n  Requested:   ${asked}` +
        `\n  Staged from: ${built}\n` +
        "  Most likely another crate built this same cdylib with different\n" +
        "  features and uplifted it over the top."
    );
  }
  console.log(`Features verified: ${asked}`);
}

if (debug) {
  console.log(
    "\nNote: this is a debug build. They are enormous once a real engine is\n" +
      "linked (~1.8 GB reported for the prototype) — do not ship or sideload one."
  );
}
