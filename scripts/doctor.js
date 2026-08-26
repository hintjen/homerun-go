#!/usr/bin/env node
/**
 * Reports what you can build on this machine, and what is missing.
 *
 *   node scripts/doctor.js            everything
 *   node scripts/doctor.js ios        just that platform's needs
 *
 * The native toolchains fail unhelpfully — a missing Rust target surfaces
 * as a linker error about a file you never mentioned — so check first.
 */
const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const { ROOT, PLATFORM_TARGETS, TARGETS, UI_DESTINATIONS } = require("./targets");
const { JDK_MIN, JDK_MAX, findJdk, INSTALL_HINT } = require("./jdk");

const requested = process.argv.slice(2).filter((a) => !a.startsWith("-"));
const platforms = requested.length ? requested : ["ios", "android"];

function capture(cmd, args) {
  try {
    return execFileSync(cmd, args, {
      encoding: "utf8",
      shell: true,
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return null;
  }
}

const rows = [];
const add = (ok, name, detail, fix) => rows.push({ ok, name, detail, fix });

// --- shared ---------------------------------------------------------------

const cargo = capture("cargo", ["--version"]);
add(
  Boolean(cargo),
  "Rust toolchain",
  cargo || "cargo not on PATH",
  "Install from https://rustup.rs — it lives in ~/.cargo/bin, which must be on PATH"
);

const node = process.version;
add(true, "Node", node, "");

// `device-ws` pulls in aws-lc-sys, which compiles C through cmake. Without it
// the failure names aws-lc-sys and a build script, which reads as a broken
// dependency rather than a missing tool — and it only appears on a phone
// target, since the host build has the feature off.
const cmake = capture("cmake", ["--version"]);
add(
  Boolean(cmake),
  "cmake",
  cmake ? cmake.split("\n")[0] : "not found",
  "brew install cmake   (aws-lc-sys builds C for the device websocket's TLS)"
);

// The UI is a published bundle fetched from the CDN by `npm run ui:<platform>`,
// not a dependency of this repo. Gradle and Xcode package whatever sits in the
// destination — or refuse to, after every other artefact has already built —
// so say up front whether one is staged.
for (const platform of platforms) {
  const dest = UI_DESTINATIONS[platform];
  if (!dest) continue;
  const staged = fs.existsSync(path.join(dest, "index.html"));
  add(
    staged,
    `Shared UI staged (${platform})`,
    staged ? path.relative(ROOT, dest) : "not staged",
    `npm run ui:${platform}   (fetches and verifies the published bundle from the CDN)`
  );
}

const installedTargets = (
  capture("rustup", ["target", "list", "--installed"]) || ""
).split(/\r?\n/);

// --- the tunnel, both platforms --------------------------------------------
//
// Without it a hosted server is reachable only on the phone's own Wi-Fi — on
// cellular, CGNAT means nobody can join at all. Android execs the binary,
// iOS links a gomobile framework; both are built from the same fork checkout.

const go = capture("go", ["version"]);
add(
  Boolean(go),
  "Go",
  go || "not found",
  "brew install go / winget install GoLang.Go   (the wireproxy fork needs Go 1.26+)"
);
const wireproxySrc = process.env.HOMERUN_WIREPROXY_SRC
  ? path.resolve(ROOT, process.env.HOMERUN_WIREPROXY_SRC)
  : path.join(path.dirname(ROOT), "wireproxy-homerun");
const wireproxyPresent = fs.existsSync(path.join(wireproxySrc, "go.mod"));
add(
  wireproxyPresent,
  "wireproxy fork",
  wireproxyPresent ? wireproxySrc : "not found",
  "git clone git@github.com:hintjen/wireproxy-homerun.git   (as a sibling of this repo,\n" +
    "    or set HOMERUN_WIREPROXY_SRC)"
);
// The build refuses a checkout that is not at the pinned revision, so say so
// here rather than at the end of a Gradle run.
const wireproxyPinFile = path.join(ROOT, "scripts", "wireproxy.rev");
const wireproxyPinned = fs.existsSync(wireproxyPinFile)
  ? fs.readFileSync(wireproxyPinFile, "utf8").trim()
  : "";
// `git -C` rather than a cwd option: capture() has none, and passing one
// silently reported this repository's HEAD as the fork's.
const wireproxyActual = wireproxyPresent
  ? capture("git", ["-C", `"${wireproxySrc}"`, "rev-parse", "HEAD"])
  : null;
add(
  Boolean(wireproxyActual) && wireproxyActual === wireproxyPinned,
  "wireproxy fork revision",
  wireproxyActual
    ? `${wireproxyActual.slice(0, 12)}${wireproxyActual === wireproxyPinned ? "" : ` (pinned: ${wireproxyPinned.slice(0, 12)})`}`
    : "unknown",
  `git -C "${wireproxySrc}" fetch origin && git -C "${wireproxySrc}" checkout --detach ${wireproxyPinned || "<scripts/wireproxy.rev>"}`
);

// --- per platform ---------------------------------------------------------

for (const platform of platforms) {
  const targets = PLATFORM_TARGETS[platform];
  if (!targets) {
    console.error(`Unknown platform "${platform}". Expected: ios, android`);
    process.exit(2);
  }

  if (platform === "ios" && process.platform !== "darwin") {
    add(
      false,
      "iOS builds",
      `not possible on ${process.platform}`,
      "iOS needs macOS (Xcode's linker and SDKs). Build it on a Mac or CI runner."
    );
    continue;
  }

  if (platform === "ios") {
    const xcode = capture("xcodebuild", ["-version"]);
    add(
      Boolean(xcode),
      "Xcode",
      xcode ? xcode.split("\n")[0] : "xcodebuild not found",
      "Install Xcode from the App Store, then: xcode-select --install"
    );
    const xcodegen = capture("xcodegen", ["--version"]);
    add(
      Boolean(xcodegen),
      "XcodeGen",
      xcodegen || "not found",
      "brew install xcodegen   (the .xcodeproj is generated, not committed)"
    );

    // iOS gets the tunnel as a gomobile framework, because it cannot spawn
    // the binary. Go itself and the fork checkout are checked for both
    // platforms above.
    const gomobile = capture("gomobile", ["version"]);
    add(
      Boolean(gomobile),
      "gomobile",
      gomobile ? "installed" : "not found",
      "go install golang.org/x/mobile/cmd/gomobile@latest && gomobile init\n" +
        "    It lands in $(go env GOPATH)/bin — put that on PATH."
    );
  }

  if (platform === "android") {
    const sdk = process.env.ANDROID_HOME || process.env.ANDROID_SDK_ROOT;
    add(
      Boolean(sdk && fs.existsSync(sdk)),
      "Android SDK",
      sdk || "ANDROID_HOME / ANDROID_SDK_ROOT not set",
      "Install the SDK, then set ANDROID_HOME (and ANDROID_SDK_ROOT) to it"
    );

    // Checked by version, not just presence: JAVA_HOME usually points at
    // Studio's bundled JBR, which is routinely too new for AGP. Same search
    // the build script uses, so the doctor and the build never disagree.
    const { jdk, seen } = findJdk();
    add(
      Boolean(jdk),
      `JDK ${JDK_MIN}-${JDK_MAX}`,
      jdk
        ? `${jdk.major} at ${jdk.home}`
        : seen.length
          ? `only saw ${seen.map((s) => s.major).join(", ")}`
          : "no JDK found",
      INSTALL_HINT
    );

    const emulator = Boolean(sdk) && fs.existsSync(path.join(sdk, "emulator"));
    add(
      emulator,
      "Emulator",
      emulator ? "installed" : "not installed",
      'sdkmanager "emulator" "system-images;android-36;google_apis;x86_64"\n' +
        "    then: avdmanager create avd -n homerun_api36 " +
        '-k "system-images;android-36;google_apis;x86_64" -d pixel_7'
    );

    const ndkVar = process.env.ANDROID_NDK_HOME || process.env.ANDROID_NDK_ROOT;
    add(
      Boolean(ndkVar),
      "Android NDK",
      ndkVar || "ANDROID_NDK_HOME / ANDROID_NDK_ROOT not set",
      "Android Studio > SDK Manager > SDK Tools > NDK, then export ANDROID_NDK_HOME"
    );
    const ndk = capture("cargo", ["ndk", "--version"]);
    add(
      Boolean(ndk),
      "cargo-ndk",
      ndk || "not installed",
      "cargo install cargo-ndk"
    );
  }

  for (const key of targets) {
    const triple = TARGETS[key].triple;
    const have = installedTargets.includes(triple);
    add(have, `Rust target: ${key}`, triple, `rustup target add ${triple}`);
  }
}

// --- report ---------------------------------------------------------------

const pad = Math.max(...rows.map((r) => r.name.length));
console.log("");
for (const r of rows) {
  console.log(`${r.ok ? "OK  " : "MISS"}  ${r.name.padEnd(pad)}  ${r.detail}`);
}

const missing = rows.filter((r) => !r.ok);
if (missing.length) {
  console.log("\nTo fix:\n");
  for (const r of missing) console.log(`  ${r.name}\n    ${r.fix}\n`);
  console.log(`${missing.length} item(s) missing.`);
  process.exit(1);
}

console.log("\nEverything needed is present.");
