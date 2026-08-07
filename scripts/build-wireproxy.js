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
 * Source is the private fork `hintjen/wireproxy-fork`, which adds the
 * `[UDPServerTunnel]` section upstream does not have. Bedrock, crossplay and
 * voice chat all need it. Checked out as a sibling of this repo, or pointed
 * at with HOMERUN_WIREPROXY_SRC.
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
    // Go links android/arm64 internally, so this one needs no NDK.
    cgo: false,
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
    : path.join(path.dirname(ROOT), "wireproxy-fork");

  if (!fs.existsSync(path.join(candidate, "wireproxy", "go.mod"))) {
    fail(
      `No wireproxy-fork checkout at ${candidate}.\n\n` +
        "  git clone git@github.com:hintjen/wireproxy-fork.git\n\n" +
        "next to this repo, or set HOMERUN_WIREPROXY_SRC to where it is."
    );
  }
  return candidate;
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
 * The NDK's clang for the emulator build. Only android/amd64 needs it, which
 * is why a device build works on a machine with no NDK at all.
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
      `\t${relative(path.join(source, "wireproxy"))}`,
      `\t${relative(path.join(source, "wireguard-go"))}`,
      ")",
      "",
      "// The fork's wireguard-go sits on a 2023 upstream commit whose netstack",
      "// does not compile against a newer gvisor (`pkt.IsNil undefined`). Both",
      "// fork modules already ask for this version; pinning it here stops a",
      "// stray `go mod tidy` in the binding from raising the workspace maximum.",
      "//",
      "// Never `go work sync` here: it writes resolved versions back into the",
      "// fork's own go.mod files — a change to another repository, and exactly",
      "// the upgrade that breaks the build.",
      "replace gvisor.dev/gvisor => gvisor.dev/gvisor v0.0.0-20230927004350-cbd86285d259",
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

const moduleDir = path.join(source, "wireproxy");

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
      "-ldflags", `"-s -w -X 'main.version=${version}'"`,
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

const size = (fs.statSync(outFile).size / 1024 / 1024).toFixed(1);
console.log(`\nwireproxy ${version} -> ${target.abi}, ${size} MB`);
console.log(`  ${elf.machine}, PIE, interpreter ${elf.interpreter}\n`);

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
  for (let i = 0; i < phnum; i++) {
    const ph = phoff + i * phentsize;
    if (buf.readUInt32LE(ph) !== 3) continue; // PT_INTERP
    const offset = Number(buf.readBigUInt64LE(ph + 8));
    const filesz = Number(buf.readBigUInt64LE(ph + 32));
    interpreter = buf.toString("utf8", offset, offset + filesz).replace(/\0+$/, "");
    break;
  }
  return { machine, pie, interpreter };
}
