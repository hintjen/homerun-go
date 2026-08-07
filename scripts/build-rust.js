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
const fs = require("fs");
const path = require("path");

const { CRATE, TARGETS } = require("./targets");

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
    cwd: CRATE,
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

// --- build ----------------------------------------------------------------

console.log(`\nBuilding ${target.label} (${profile})\n`);

const profileArgs = debug ? [] : ["--release"];

// The device builds link the real server; the host build deliberately does
// not, so `cargo test` stays a couple of seconds and needs no Pumpkin. Pass
// --stub to cross-compile without the engine, which is how you check that the
// FFI surface itself still builds for a target without waiting for wasmtime.
const engineArgs = args.includes("--stub") ? [] : ["--features", "pumpkin-engine"];

if (target.kind === "host") {
  run("cargo", ["build", ...profileArgs]);
  console.log("\nHost build done. `cargo test` runs the suite.");
  process.exit(0);
}

if (target.kind === "ndk") {
  // cargo-ndk wants its own flags before `build`.
  run("cargo", ["ndk", "-t", target.abi, "build", ...profileArgs, ...engineArgs]);
} else {
  run("cargo", [
    "build",
    ...profileArgs,
    ...engineArgs,
    "--target",
    target.triple,
  ]);
}

// --- stage the artifact ---------------------------------------------------

const built = path.join(CRATE, "target", target.triple, profile, target.artifact);
if (!fs.existsSync(built)) {
  fail(
    `Build reported success but ${target.artifact} is not at:\n  ${built}\n` +
      "  Check the crate-type in Cargo.toml (staticlib for iOS, cdylib for Android)."
  );
}

fs.mkdirSync(target.outDir, { recursive: true });
const dest = path.join(target.outDir, target.artifact);
fs.copyFileSync(built, dest);

const mb = (fs.statSync(dest).size / 1024 / 1024).toFixed(1);
console.log(`\n${target.artifact} -> ${dest}  (${mb} MB)`);

if (debug) {
  console.log(
    "\nNote: this is a debug build. They are enormous once a real engine is\n" +
      "linked (~1.8 GB reported for the prototype) — do not ship or sideload one."
  );
}
