#!/usr/bin/env node
/**
 * Builds wireproxy for an Android ABI and puts it where the APK packager
 * will find it.
 *
 *   node scripts/build-wireproxy.js android
 *   node scripts/build-wireproxy.js android-x86_64
 *
 * wireproxy is what makes a phone-hosted server reachable. A phone on
 * cellular sits behind CGNAT, so there is no port-forwarding fallback the
 * way there is on desktop — without the tunnel nobody can join.
 *
 * Source is the fork `hintjen/wireproxy-homerun` — upstream wireproxy made safe
 * to link into an application, plus the `[UDPServerTunnel]` section upstream
 * does not have, which Bedrock, crossplay and voice chat all need. Its
 * FORK.md is the account. Checked out as a sibling of this repo, or pointed
 * at with HOMERUN_WIREPROXY_SRC.
 *
 * **The checkout must be at the revision in `scripts/wireproxy.rev`.** The
 * fork merges upstream on a schedule, so its `main` moves without any commit
 * here; the pin is what keeps a store build from shipping whatever landed
 * overnight. Bumping it is a reviewed commit, the same as the Pumpkin rev.
 * HOMERUN_WIREPROXY_ALLOW_UNPINNED=1 skips the check for local iteration on
 * the fork itself.
 */
const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");

/**
 * `GOOS=android`, never `GOOS=linux`.
 *
 * A linux/arm64 PIE binary builds without a murmur and then will not start
 * on a device: Go stamps PT_INTERP as `/lib/ld-linux-aarch64.so.1`, a glibc
 * path bionic does not have. `GOOS=android` emits `/system/bin/linker64`,
 * and is PIE by default — which API 21+ requires in order to exec at all.
 * The check at the bottom of this file asserts it rather than trusting it.
 */
const TARGETS = {
  android: {
    label: "Android arm64 (devices)",
    goarch: "arm64",
    abi: "arm64-v8a",
    // cgo, and it is not optional: **DNS does not work without it.**
    //
    // Android ships no /etc/resolv.conf, so Go's pure-Go resolver has no
    // nameservers to read and falls back to 127.0.0.1:53 — every lookup fails
    // with "connection refused", and the gateway is reached by hostname.
    //
    // Go links android/arm64 internally and needs no C toolchain, so this was
    // `false` and looked right. It was only ever exercised on the emulator,
    // which is x86_64 and had cgo forced on for an unrelated linking reason —
    // so the configuration that worked was the one that never ships. The same
    // mistake in build-restic.js is what stopped a server ever starting on a
    // real phone.
    cgo: true,
    ndkTriple: "aarch64-linux-android26-clang",
    interpreter: "/system/bin/linker64",
    machine: "AArch64",
  },
  /**
   * iOS gets a gomobile framework rather than a binary, because the platform
   * cannot spawn one. The tunnel runs inside the app, which is also what keeps
   * it free of any VPN profile: wireproxy terminates WireGuard in its own
   * userspace netstack and never registers an interface with the OS.
   */
  ios: {
    label: "iOS device + simulator (xcframework)",
    kind: "gomobile",
    requiresMacOS: true,
  },
  "android-x86_64": {
    label: "Android x86_64 (emulator)",
    goarch: "amd64",
    abi: "x86_64",
    // "android/amd64 requires external (cgo) linking" — the one target that
    // needs a C toolchain. Emulator-only, so it never touches the ship path.
    cgo: true,
    ndkTriple: "x86_64-linux-android26-clang",
    interpreter: "/system/bin/linker64",
    machine: "X86-64",
  },
};

const name = process.argv[2];
const target = TARGETS[name];
if (!target) {
  console.error(
    "usage: build-wireproxy.js <target>\n\ntargets:\n" +
      Object.entries(TARGETS)
        .map(([k, t]) => `  ${k.padEnd(16)} ${t.label}`)
        .join("\n")
  );
  process.exit(2);
}

/** The fork checkout: an explicit override, else a sibling of this repo. */
function sourceDir() {
  const override = process.env.HOMERUN_WIREPROXY_SRC;
  const candidate = override
    ? path.resolve(ROOT, override)
    : path.join(path.dirname(ROOT), "wireproxy-homerun");

  if (!fs.existsSync(path.join(candidate, "go.mod"))) {
    fail(
      `No wireproxy-homerun checkout at ${candidate}.\n\n` +
        "  git clone https://github.com/hintjen/wireproxy-homerun.git\n\n" +
        "next to this repo, or set HOMERUN_WIREPROXY_SRC to where it is.\n" +
        "(A checkout with a wireproxy/ subdirectory is the old monorepo layout;\n" +
        "pull main.)"
    );
  }

  const pinned = pinnedRev();
  const actual = capture("git", ["rev-parse", "HEAD"], { cwd: candidate });
  if (actual !== pinned) {
    if (process.env.HOMERUN_WIREPROXY_ALLOW_UNPINNED) {
      console.warn(
        `\nwireproxy checkout is at ${(actual || "?").slice(0, 12)}, not the pinned ` +
          `${pinned.slice(0, 12)} — HOMERUN_WIREPROXY_ALLOW_UNPINNED is set, building anyway.\n`
      );
    } else {
      fail(
        `The wireproxy checkout at ${candidate} is at\n` +
          `  ${actual || "(not a git checkout)"}\n` +
          `but scripts/wireproxy.rev pins\n  ${pinned}\n\n` +
          `  git -C "${candidate}" fetch origin && git -C "${candidate}" checkout --detach ${pinned}\n\n` +
          "or, to bump the pin, put the new revision in scripts/wireproxy.rev and\n" +
          "commit it. HOMERUN_WIREPROXY_ALLOW_UNPINNED=1 builds from whatever is\n" +
          "checked out, for iterating on the fork."
      );
    }
  }
  return candidate;
}

/** The revision this repository builds the tunnel from. One line, a full SHA. */
function pinnedRev() {
  const file = path.join(ROOT, "scripts", "wireproxy.rev");
  const rev = fs.existsSync(file) ? fs.readFileSync(file, "utf8").trim() : "";
  if (!/^[0-9a-f]{40}$/.test(rev)) {
    fail(`scripts/wireproxy.rev must hold one full commit SHA; found ${JSON.stringify(rev)}.`);
  }
  return rev;
}

function fail(message) {
  console.error(`\n${message}\n`);
  process.exit(1);
}

function capture(cmd, args, opts = {}) {
  try {
    return execFileSync(cmd, args, { encoding: "utf8", shell: true, ...opts }).trim();
  } catch {
    return null;
  }
}

/** Go, from PATH or the portable install this machine's other tooling uses. */
function goBinary() {
  if (capture("go", ["version"])) return "go";
  const portable = path.join(
    process.env.USERPROFILE || process.env.HOME || "",
    "tools", "go", "bin", "go.exe"
  );
  if (fs.existsSync(portable)) return portable;
  fail(
    "Go is not installed.\n\n" +
      "  winget install GoLang.Go\n\n" +
      "or unpack a release into ~/tools/go. The fork needs Go 1.26+."
  );
}

/**
 * The NDK's clang. Every Android target needs one now — cgo is what gives Go
 * a working DNS resolver on Android, so a device build no longer builds on a
 * machine with no NDK. See the `cgo` note in TARGETS.
 */
function ndkCompiler() {
  const home =
    process.env.ANDROID_NDK_HOME ||
    process.env.ANDROID_NDK_ROOT ||
    latestNdkUnderSdk();
  if (!home) {
    fail(
      "The x86_64 build needs the NDK — Go reports\n" +
        "  android/amd64 requires external (cgo) linking\n\n" +
        "Set ANDROID_NDK_HOME, or build the arm64 target instead, which needs no NDK."
    );
  }
  const hosts = ["windows-x86_64", "linux-x86_64", "darwin-x86_64"];
  for (const host of hosts) {
    const base = path.join(home, "toolchains", "llvm", "prebuilt", host, "bin");
    // Windows ships .cmd shims next to the extensionless drivers.
    for (const suffix of [".cmd", ""]) {
      const candidate = path.join(base, target.ndkTriple + suffix);
      if (fs.existsSync(candidate)) return candidate;
    }
  }
  fail(`Found an NDK at ${home} but no ${target.ndkTriple} in it.`);
}

function latestNdkUnderSdk() {
  const sdk =
    process.env.ANDROID_HOME ||
    process.env.ANDROID_SDK_ROOT ||
    path.join(process.env.LOCALAPPDATA || "", "Android", "Sdk");
  const dir = path.join(sdk, "ndk");
  if (!fs.existsSync(dir)) return null;
  const versions = fs.readdirSync(dir).sort();
  return versions.length ? path.join(dir, versions[versions.length - 1]) : null;
}

/**
 * Build the iOS framework with gomobile.
 *
 * The binding lives in this repo (`go/wireproxy-ios/`) rather than in the
 * fork: it is iOS-only glue, and the fork is shared with desktop and Android.
 * It reaches the fork through a *generated* workspace, because the fork's
 * location is configurable and so cannot be a committed `replace`.
 */
function buildXCFramework(source, go) {
  const gomobile = capture("gomobile", ["version"])
    ? "gomobile"
    : path.join(process.env.HOME || "", "go", "bin", "gomobile");
  if (!fs.existsSync(gomobile) && gomobile !== "gomobile") {
    fail(
      "gomobile is not installed.\n\n" +
        "  go install golang.org/x/mobile/cmd/gomobile@latest\n" +
        "  gomobile init\n\n" +
        "It lives in $(go env GOPATH)/bin, which must be on PATH."
    );
  }

  const moduleDir = path.join(ROOT, "go", "wireproxy-ios");
  const relative = (p) => path.relative(moduleDir, p).split(path.sep).join("/");

  // Regenerated every build so a moved fork checkout cannot leave a stale
  // path behind. Gitignored.
  fs.writeFileSync(
    path.join(moduleDir, "go.work"),
    [
      "// Generated by scripts/build-wireproxy.js — do not edit, do not commit.",
      "go 1.26.5",
      "",
      "use (",
      "\t.",
      `\t${relative(source)}`,
      ")",
      "",
      "// wireguard-go and gvisor come from the module proxy at the highest",
      "// version either module asks for — the fork no longer vendors them.",
      "//",
      "// Never `go work sync` here: it writes resolved versions back into the",
      "// fork's own go.mod — a change to another repository.",
      "",
    ].join("\n")
  );

  const outDir = path.join(ROOT, "ios", "HomerunHost", "lib");
  const outFile = path.join(outDir, "WireproxyIOS.xcframework");
  fs.mkdirSync(outDir, { recursive: true });

  console.log(`\nBuilding wireproxy for ${target.label}`);
  console.log(`  source  ${source}`);
  console.log(`  binding ${moduleDir}`);
  console.log(`  output  ${outFile}\n`);

  // Into a temp directory first: a mid-build failure must not delete the
  // framework Xcode is expecting, or every subsequent build fails for a
  // second, unrelated reason.
  const staging = fs.mkdtempSync(path.join(require("os").tmpdir(), "wireproxy-ios-"));
  try {
    execFileSync(
      gomobile,
      [
        "bind",
        "-target", "ios,iossimulator",
        // Drop symtab/DWARF and normalise paths. The Go runtime's pclntab is
        // not removable, so this is a trim rather than a transformation.
        "-ldflags", '"-s -w"',
        "-trimpath",
        "-o", `"${path.join(staging, "WireproxyIOS.xcframework")}"`,
        ".",
      ],
      { stdio: "inherit", shell: true, cwd: moduleDir }
    );
  } catch {
    fs.rmSync(staging, { recursive: true, force: true });
    fail("gomobile bind failed — see the output above.");
  }

  fs.rmSync(outFile, { recursive: true, force: true });
  fs.renameSync(path.join(staging, "WireproxyIOS.xcframework"), outFile);
  fs.rmSync(staging, { recursive: true, force: true });

  const slices = fs.readdirSync(outFile).filter((e) => e.startsWith("ios-"));
  if (slices.length < 2) {
    fail(`Expected a device and a simulator slice, got: ${slices.join(", ") || "none"}`);
  }

  console.log(`\nWireproxyIOS.xcframework -> ${outFile}`);
  slices.forEach((slice) => console.log(`  ${slice}`));
}

// ---------------------------------------------------------------------------

const source = sourceDir();
const go = goBinary();

if (target.requiresMacOS && process.platform !== "darwin") {
  fail(
    `${target.label} can only be built on macOS — gomobile needs Xcode's SDKs.\n` +
      `  You are on ${process.platform}.`
  );
}

if (target.kind === "gomobile") {
  buildXCFramework(source, go);
  process.exit(0);
}

const moduleDir = source;

// jniLibs is the only place API 29+ will exec from, and the packager only
// takes `lib*.so` into it. This is an executable regardless of the name.
const outDir = path.join(
  ROOT, "android", "app", "src", "main", "jniLibs", target.abi
);
const outFile = path.join(outDir, "libwireproxy.so");

const version = capture("git", ["describe", "--always", "--tags"], { cwd: source }) || "unknown";

console.log(`\nBuilding wireproxy ${version} for ${target.label}`);
console.log(`  source  ${moduleDir}`);
console.log(`  output  ${outFile}\n`);

fs.mkdirSync(outDir, { recursive: true });

const env = {
  ...process.env,
  GOOS: "android",
  GOARCH: target.goarch,
  CGO_ENABLED: target.cgo ? "1" : "0",
};
if (target.cgo) {
  env.CC = ndkCompiler();
  console.log(`  cc      ${env.CC}\n`);
}

try {
  execFileSync(
    go,
    [
      "build", "-trimpath",
      // `-extldflags` because cgo hands linking to the NDK's ld, whose default
      // is 4 KB pages — Go's own linker emits 64 KB and needs no help. Turning
      // cgo on for DNS therefore silently loses the alignment, which the check
      // after the build catches. The two belong together.
      "-ldflags",
      `"-s -w -X 'main.version=${version}'` +
        (target.cgo ? " -extldflags=-Wl,-z,max-page-size=16384" : "") + `"`,
      "-o", `"${outFile}"`,
      "./cmd/wireproxy",
    ],
    { stdio: "inherit", shell: true, cwd: moduleDir, env }
  );
} catch {
  fail("go build failed — see the output above.");
}

const elf = readElf(outFile);

// The failure this guards against is nasty: a linux/arm64 build succeeds,
// packages, installs, and then dies on the device with ENOENT for a loader
// nobody mentioned. Parsed here rather than shelled out to `readelf`, which
// is not on a stock Windows box — and `go tool readelf` does not exist, which
// is how an earlier version of this check silently passed while claiming to
// have verified something.
if (elf.machine !== target.machine) {
  fail(`Built, but it is ${elf.machine} and ${target.abi} needs ${target.machine}.`);
}
if (elf.interpreter !== target.interpreter) {
  fail(
    `Built, but its interpreter is ${elf.interpreter || "absent"}, not ${target.interpreter}.\n` +
      "That binary would not exec on a device. Check GOOS is android, not linux."
  );
}
if (!elf.pie) {
  fail("Built, but it is not PIE. Android has refused to exec non-PIE binaries since API 21.");
}
if (elf.pageAlign < 16 * 1024) {
  fail(
    `Built, but its LOAD segments are aligned to ${elf.pageAlign} bytes and Android needs\n` +
      "at least 16384. New 64-bit devices run 16 KB pages and their linker refuses\n" +
      "anything coarser — it would install and fail to start, with no useful error.\n" +
      "Play has required 16 KB support for targetSdk 35+ since 1 November 2025.\n\n" +
      "Rebuild with -ldflags=\"-extldflags=-Wl,-z,max-page-size=16384\"."
  );
}

const size = (fs.statSync(outFile).size / 1024 / 1024).toFixed(1);
console.log(`\nwireproxy ${version} -> ${target.abi}, ${size} MB`);
console.log(`  ${elf.machine}, PIE, interpreter ${elf.interpreter}, ${elf.pageAlign / 1024} KB page aligned\n`);

/**
 * The three things about this binary that have to be true for Android to
 * exec it: the architecture, the loader path, and PIE.
 */
function readElf(file) {
  const buf = fs.readFileSync(file);
  if (buf.length < 64 || buf.readUInt32BE(0) !== 0x7f454c46) {
    fail(`${file} is not an ELF file.`);
  }
  if (buf[4] !== 2) fail("Expected a 64-bit ELF.");

  const MACHINES = { 0x3e: "X86-64", 0xb7: "AArch64" };
  const type = buf.readUInt16LE(16);
  const machine = MACHINES[buf.readUInt16LE(18)] ?? `unknown (0x${buf.readUInt16LE(18).toString(16)})`;

  // ET_DYN. A Go PIE binary is ET_DYN with a PT_INTERP; so is a shared
  // library, but nothing here builds one.
  const pie = type === 3;

  const phoff = Number(buf.readBigUInt64LE(32));
  const phentsize = buf.readUInt16LE(54);
  const phnum = buf.readUInt16LE(56);

  let interpreter = null;
  const aligns = [];
  for (let i = 0; i < phnum; i++) {
    const ph = phoff + i * phentsize;
    const ptype = buf.readUInt32LE(ph);
    if (ptype === 1) aligns.push(Number(buf.readBigUInt64LE(ph + 48))); // PT_LOAD
    if (ptype !== 3) continue; // PT_INTERP
    const offset = Number(buf.readBigUInt64LE(ph + 8));
    const filesz = Number(buf.readBigUInt64LE(ph + 32));
    interpreter = buf.toString("utf8", offset, offset + filesz).replace(/\0+$/, "");
  }
  // The coarsest page size this can load under. Go emits 64 KB by default,
  // which already covers Android's 16 KB devices — checked so that a toolchain
  // or flag change quietly dropping it fails here rather than on a phone. The
  // JRE's libandroid-spawn.so did exactly that; see stage-jre.py.
  return { machine, pie, interpreter, pageAlign: aligns.length ? Math.min(...aligns) : 0 };
}
