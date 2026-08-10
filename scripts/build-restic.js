#!/usr/bin/env node
/**
 * Build restic for Android and stage it into `jniLibs`.
 *
 *   node scripts/build-restic.js android            # arm64, devices
 *   node scripts/build-restic.js android-x86_64     # emulator
 *
 * # Why we build it rather than download it
 *
 * restic publishes no android build. Its linux binaries are linked against
 * glibc and carry `/lib/ld-linux-aarch64.so.1` as their interpreter, which
 * does not exist on Android — the binary is well formed and simply refuses to
 * start. `GOOS=android` emits Bionic's linker instead.
 *
 * # Why it is named `librestic.so`
 *
 * It is an executable, not a library. API 29+ permits exec only from
 * `nativeLibraryDir`, and the APK packager only extracts files matching
 * `lib*.so` there. The name is the price of being allowed to run at all —
 * `libwireproxy.so` is the same trick for the same reason.
 *
 * # The version is pinned, and pinned to match the desktop
 *
 * `go/restic/` holds a module whose only job is to name a restic version. The
 * desktop's `download-assets.js` pins the same one. Both write to the same
 * repositories, so bumping one alone is how two clients end up disagreeing
 * about a format.
 */
const { execFileSync, execSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const MODULE_DIR = path.join(ROOT, "go", "restic");
const PACKAGE = "github.com/restic/restic/cmd/restic";

const TARGETS = {
  android: {
    label: "Android arm64 (devices)",
    goarch: "arm64",
    abi: "arm64-v8a",
    // Go links android/arm64 internally; no NDK needed.
    cgo: false,
    machine: "AArch64",
  },
  "android-x86_64": {
    label: "Android x86_64 (emulator)",
    goarch: "amd64",
    abi: "x86_64",
    // "android/amd64 requires external (cgo) linking" — the one target that
    // needs a C toolchain. Emulator-only, so it never touches the ship path.
    cgo: true,
    machine: "x86-64",
  },
};

function fail(message) {
  console.error(`\n${message}\n`);
  process.exit(1);
}

function capture(command, args) {
  try {
    return execFileSync(command, args, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });
  } catch {
    return null;
  }
}

/** The Go toolchain, from PATH or the vendored copy the other scripts use. */
function goBinary() {
  if (capture("go", ["version"])) return "go";
  const vendored = path.join(process.env.HOME || process.env.USERPROFILE || "", "tools", "go", "bin", "go.exe");
  if (fs.existsSync(vendored)) return vendored;
  fail("Go is not installed.\n\n  Install Go 1.25+, or unpack a release into ~/tools/go.");
}

/** The NDK's clang, needed only by the emulator target. */
function ndkCompiler() {
  const home =
    process.env.ANDROID_NDK_HOME ||
    latestNdkUnderSdk() ||
    fail(
      "The x86_64 target needs a C compiler:\n\n" +
        "  android/amd64 requires external (cgo) linking\n\n" +
        "Set ANDROID_NDK_HOME, or build the arm64 target, which needs no NDK."
    );
  const host = process.platform === "win32" ? "windows-x86_64"
    : process.platform === "darwin" ? "darwin-x86_64" : "linux-x86_64";
  const base = path.join(home, "toolchains", "llvm", "prebuilt", host, "bin");
  const suffix = process.platform === "win32" ? ".cmd" : "";
  const compiler = path.join(base, `x86_64-linux-android26-clang${suffix}`);
  if (!fs.existsSync(compiler)) fail(`No clang for x86_64 at:\n  ${compiler}`);
  return compiler;
}

function latestNdkUnderSdk() {
  const sdk =
    process.env.ANDROID_SDK_ROOT ||
    process.env.ANDROID_HOME ||
    path.join(process.env.LOCALAPPDATA || "", "Android", "Sdk");
  const dir = path.join(sdk, "ndk");
  if (!fs.existsSync(dir)) return null;
  const versions = fs.readdirSync(dir).sort();
  return versions.length ? path.join(dir, versions[versions.length - 1]) : null;
}

/**
 * Assert the binary Android will actually be asked to run.
 *
 * A wrong GOOS produces a file that looks fine and cannot start, so this is
 * checked rather than trusted. `go tool readelf` does not exist, hence the
 * hand-rolled program-header walk.
 */
function readElf(file) {
  const buffer = fs.readFileSync(file);
  if (buffer.slice(0, 4).toString("binary") !== "\x7fELF") fail(`${file} is not an ELF file.`);

  const type = buffer.readUInt16LE(16);
  const machine = buffer.readUInt16LE(18);
  const phoff = Number(buffer.readBigUInt64LE(32));
  const phentsize = buffer.readUInt16LE(54);
  const phnum = buffer.readUInt16LE(56);

  let interpreter = null;
  for (let i = 0; i < phnum; i++) {
    const offset = phoff + i * phentsize;
    if (buffer.readUInt32LE(offset) !== 3 /* PT_INTERP */) continue;
    const at = Number(buffer.readBigUInt64LE(offset + 8));
    const size = Number(buffer.readBigUInt64LE(offset + 32));
    interpreter = buffer.toString("utf8", at, at + size).replace(/\0+$/, "");
  }

  return {
    pie: type === 3,
    machine: machine === 0xb7 ? "AArch64" : machine === 0x3e ? "x86-64" : `unknown (0x${machine.toString(16)})`,
    interpreter,
  };
}

const name = process.argv[2];
const target = TARGETS[name];
if (!target) {
  fail(
    "usage: build-restic.js <target>\n\ntargets:\n" +
      Object.entries(TARGETS).map(([key, t]) => `  ${key.padEnd(16)} ${t.label}`).join("\n")
  );
}

const go = goBinary();
const version = (() => {
  const gomod = fs.readFileSync(path.join(MODULE_DIR, "go.mod"), "utf8");
  return (gomod.match(/github\.com\/restic\/restic (v[\d.]+)/) || [, "unknown"])[1];
})();

const outDir = path.join(ROOT, "android", "app", "src", "main", "jniLibs", target.abi);
fs.mkdirSync(outDir, { recursive: true });
const outFile = path.join(outDir, "librestic.so");

console.log(`\nBuilding restic ${version} for ${target.label}`);
console.log(`  module  ${MODULE_DIR}`);
console.log(`  output  ${outFile}`);

const env = {
  ...process.env,
  GOOS: "android",
  GOARCH: target.goarch,
  CGO_ENABLED: target.cgo ? "1" : "0",
};
if (target.cgo) {
  const compiler = ndkCompiler();
  env.CC = compiler;
  console.log(`  cc      ${compiler}`);
}

try {
  execFileSync(
    go,
    ["build", "-trimpath", "-ldflags=-s -w", "-o", outFile, PACKAGE],
    { cwd: MODULE_DIR, env, stdio: "inherit" }
  );
} catch {
  fail("go build failed — see the output above.");
}

const elf = readElf(outFile);
const size = (fs.statSync(outFile).size / (1024 * 1024)).toFixed(1);
console.log(`\nrestic ${version} -> ${target.abi}, ${size} MB`);
console.log(`  ${elf.machine}, ${elf.pie ? "PIE" : "NOT PIE"}, interpreter ${elf.interpreter}`);

if (!elf.pie) fail("Not a PIE binary. Android will refuse to exec it.");
if (elf.machine !== target.machine) fail(`Wrong architecture: ${elf.machine}, expected ${target.machine}.`);
if (elf.interpreter !== "/system/bin/linker64") {
  fail(
    `Wrong interpreter: ${elf.interpreter}\n\n` +
      "That path does not exist on Android — the binary would fail to start with no\n" +
      "useful error. This is what GOOS=linux produces; GOOS=android is required."
  );
}
