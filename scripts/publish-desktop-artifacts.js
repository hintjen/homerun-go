#!/usr/bin/env node
/**
 * Prepare manifests from final, signed bytes. Print upload commands; never upload.
 *
 * # Channels
 *
 * Every published location hangs off one prefix: the immutable object keys, the
 * `latest.json` each manifest lands at, the absolute `url` written inside it,
 * and the mutable `assets/homerun_core.node` alias. `--channel` picks it, and
 * the default is `dev`.
 *
 * `prod` is the prefix installed desktops read on every server launch, so a
 * publish there reaches every player within one launch. Defaulting to it would
 * mean a forgotten argument ships an engine; defaulting to `dev` means a
 * forgotten argument publishes somewhere nobody is listening. `prod` reproduces
 * the previous layout exactly -- same keys, same absolute URLs -- so switching
 * to it is not a migration.
 *
 * An unrecognised channel is refused rather than guessed at, because the guess
 * that matters is the one that resolves to `prod`.
 *
 * The dev prefix is `homerun-desktop-dev`, deliberately NOT a directory under
 * `homerun-desktop`: a nested prefix is covered by any IAM statement or bucket
 * policy scoped to `homerun-desktop/*`, and this wants to be the kind of thing
 * that fails with AccessDenied rather than the kind that silently inherits
 * production's permissions. It is also why `homerun-desktop/` -- with the
 * slash -- is the string the tests look for in dev output: `homerun-desktop`
 * without it is a substring of the dev prefix and matches both.
 *
 * # Targets
 *
 * `--target` names the desktop platform an artifact is for, in the
 * `<platform>-<arch>` form the desktop already uses (`win32-x64`,
 * `linux-x64`, ...). `win32-x64` is the default and keeps the layout every
 * released desktop reads: `pumpkin/latest.json` and its siblings, unkeyed.
 * Every other target is keyed -- `core/linux-x64/latest.json`, with the object
 * beside it -- so a manifest names one target's bytes and a Linux desktop can
 * never be handed a Windows binary by a reader that forgot to check. One
 * manifest per target rather than a map in one file, because the desktop's
 * pin is "this manifest names one object and its digest", and a map would
 * change what that means.
 *
 * Not every artifact exists for every target yet: TARGET_KINDS says which do,
 * and anything else is refused rather than published under a key nothing
 * reads.
 *
 * # --verify
 *
 * Signing rewrites the bytes, so a manifest prepared before it names a digest
 * the desktop will refuse, and refuse permanently: a checksum mismatch is not
 * retried. Nothing in the upload path notices. `--verify` re-hashes each file
 * against the manifest already on disk beside it and exits nonzero on any
 * disagreement, so a publish job can assert the order it depended on rather
 * than assume it.
 */
const { execFileSync } = require("child_process");
const os = require("os");
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const { ROOT } = require("./targets");

/**
 * The publish channels, and the one an omitted argument means.
 *
 * `HOMERUN_ARTIFACT_CHANNEL` is the environment name, and it is load-bearing
 * outside this file: `publish-desktop-artifacts.yml` in hintjen/homerun greps
 * the checked-out copy of this script for that exact string to decide whether
 * the revision it checked out understands channels at all, and refuses a dev
 * publish when it does not. Renaming it is a two-repository change.
 */
const CHANNELS = {
  prod: {
    s3: "s3://fractal-homerun/homerun-desktop",
    public: "https://fractal-homerun.s3.amazonaws.com/homerun-desktop",
  },
  dev: {
    s3: "s3://fractal-homerun/homerun-desktop-dev",
    public: "https://fractal-homerun.s3.amazonaws.com/homerun-desktop-dev",
  },
};
const DEFAULT_CHANNEL = "dev";
const CHANNEL_ENV = "HOMERUN_ARTIFACT_CHANNEL";

/**
 * Each artifact as it is published for Windows. Off Windows an executable has
 * no `.exe`, in its file name or its object key (`fileFor`, `extFor`).
 */
const LAYOUT = {
  pumpkin: { file: "homerun-desktop-minecraft-pumpkin.exe", prefix: "pumpkin", stem: "homerun-desktop-minecraft-pumpkin", ext: "exe", manifest: "pumpkin-latest.json" },
  "game-runner": { file: "homerun-game.exe", prefix: "game-runner", stem: "homerun-game", ext: "exe", manifest: "game-runner-latest.json" },
  "core-node": { file: "homerun_core.node", prefix: "core", stem: "homerun-core", ext: "node", manifest: "core-latest.json" },
  // The tunnel every hosted server is reached through, built from the fork
  // at scripts/wireproxy.rev (build-wireproxy.js, the desktop targets).
  wireproxy: { file: "homerun-desktop-server-proxy.exe", prefix: "wireproxy", stem: "homerun-desktop-server-proxy", ext: "exe", manifest: "wireproxy-latest.json" },
};

/** The artifacts each desktop target has. Pumpkin and the runner come later on macOS. */
const TARGET_KINDS = {
  "win32-x64": ["pumpkin", "game-runner", "core-node", "wireproxy"],
  "linux-x64": ["pumpkin", "game-runner", "core-node", "wireproxy"],
  "linux-arm64": ["wireproxy"],
  "darwin-arm64": ["wireproxy"],
};
const DEFAULT_TARGET = "win32-x64";

const USAGE =
  "Usage: node scripts/publish-desktop-artifacts.js " +
  "[--channel dev|prod] [--target win32-x64|linux-x64|linux-arm64|darwin-arm64] " +
  "[--only pumpkin|game-runner|core-node|wireproxy] [--verify]";

/** The released Windows layout is unkeyed; every other target is keyed. */
const isLegacyTarget = (target) => target === DEFAULT_TARGET;

function resolveTarget(requested) {
  const name = requested || DEFAULT_TARGET;
  if (!Object.hasOwn(TARGET_KINDS, name)) {
    throw new Error(`Unknown target: ${name}. Known targets: ${Object.keys(TARGET_KINDS).join(", ")}.`);
  }
  return name;
}

function fileFor(layout, target) {
  return target.startsWith("win32-") ? layout.file : layout.file.replace(/\.exe$/, "");
}

function extFor(layout, target) {
  return layout.ext === "exe" && !target.startsWith("win32-") ? "" : layout.ext;
}

/** Where a target's built artifacts are: Windows' stay where they always were. */
function artifactDir(target) {
  const base = path.join(ROOT, "dist", "desktop");
  return isLegacyTarget(target) ? base : path.join(base, target);
}

/** An explicit flag, else the environment, else dev. Never a guess. */
function resolveChannel(requested, env = {}) {
  // An empty variable is an unset one: a workflow hands an input through as ""
  // when it was left blank, and that must not resolve to a channel by accident.
  const name = requested || (env[CHANNEL_ENV] || "").trim() || DEFAULT_CHANNEL;
  if (!Object.hasOwn(CHANNELS, name)) {
    throw new Error(`Unknown channel: ${name}. Known channels: ${Object.keys(CHANNELS).join(", ")}.`);
  }
  return name;
}

function parseArgs(argv, env = {}) {
  const options = { only: null, channel: null, target: null, verify: false };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    if (arg === "--verify") {
      if (options.verify) throw new Error(USAGE);
      options.verify = true;
      continue;
    }
    const key = arg === "--only" ? "only" : arg === "--channel" ? "channel" : arg === "--target" ? "target" : null;
    if (!key) throw new Error(USAGE);
    const value = argv[++index];
    if (value === undefined || options[key] !== null) throw new Error(USAGE);
    options[key] = value;
  }
  if (options.only !== null && !Object.hasOwn(LAYOUT, options.only)) throw new Error(USAGE);
  const target = resolveTarget(options.target);
  if (options.only !== null && !TARGET_KINDS[target].includes(options.only)) {
    throw new Error(`There is no ${options.only} for ${target} yet. ${target} has: ${TARGET_KINDS[target].join(", ")}.`);
  }
  return { ...options, target, channel: resolveChannel(options.channel, env) };
}

/** What a file's final bytes say about where it is published, and as what. */
function identify(dir, kind, channel, target = DEFAULT_TARGET) {
  const layout = LAYOUT[kind];
  if (!layout) throw new Error(`Unknown artifact: ${kind}`);
  if (!TARGET_KINDS[resolveTarget(target)].includes(kind)) throw new Error(`There is no ${kind} for ${target} yet.`);
  const file = path.join(dir, fileFor(layout, target));
  const bytes = fs.readFileSync(file);
  if (!bytes.length) throw new Error(`Empty artifact: ${file}`);
  const sha256 = crypto.createHash("sha256").update(bytes).digest("hex");
  const build = sha256.slice(0, 12);
  const ext = extFor(layout, target);
  const prefix = isLegacyTarget(target) ? layout.prefix : `${layout.prefix}/${target}`;
  const objectKey = `${prefix}/${layout.stem}-${build}${ext ? `.${ext}` : ""}`;
  return {
    layout, file, bytes, sha256, build, objectKey,
    url: `${CHANNELS[channel].public}/${objectKey}`,
    manifestPath: path.join(dir, layout.manifest),
    manifestKey: `${prefix}/latest.json`,
  };
}

function prepareArtifacts(dir, selected, metadata = {}, channel = DEFAULT_CHANNEL, target = DEFAULT_TARGET) {
  resolveChannel(channel);
  resolveTarget(target);
  // Validate every input before replacing any existing manifest.
  const artifacts = selected.map((kind) => {
    const found = identify(dir, kind, channel, target);
    const info = metadata[kind] || {};
    if (kind === "pumpkin" && (!/^\d+(\.\d+){1,2}$/.test(info.minecraftVersion || "") || !Number.isInteger(info.protocol) || info.protocol <= 0)) {
      throw new Error("Pumpkin must identify its Minecraft version and protocol");
    }
    if (kind !== "pumpkin" && !info.version) throw new Error(`Missing version for ${kind}`);
    if (kind === "core-node" && (!Number.isInteger(info.abi) || info.abi < 1 || !info.sourceBuild)) {
      throw new Error("Core addon must export its ABI and source build identity");
    }
    if (kind === "wireproxy" && !/^[0-9a-f]{40}$/.test(info.rev || "")) {
      throw new Error("wireproxy must name the fork revision it was built from");
    }
    return {
      kind, channel, target, file: found.file, objectKey: found.objectKey,
      manifestPath: found.manifestPath, manifestKey: found.manifestKey,
      manifest: { ...info, build: found.build, url: found.url, sha256: found.sha256, size: found.bytes.length },
    };
  });
  for (const artifact of artifacts) {
    fs.writeFileSync(artifact.manifestPath, `${JSON.stringify(artifact.manifest, null, 2)}\n`);
  }
  return artifacts;
}

/**
 * Re-hash what is on disk against the manifest beside it.
 *
 * Returns every disagreement rather than the first: a run that signed two
 * binaries and re-prepared neither should say so once, not twice.
 */
function verifyArtifacts(dir, selected, channel = DEFAULT_CHANNEL, target = DEFAULT_TARGET) {
  resolveChannel(channel);
  resolveTarget(target);
  const problems = [];
  for (const kind of selected) {
    const layout = LAYOUT[kind];
    if (!layout) throw new Error(`Unknown artifact: ${kind}`);
    let manifest;
    try {
      manifest = JSON.parse(fs.readFileSync(path.join(dir, layout.manifest), "utf8"));
    } catch (error) {
      problems.push(`${kind}: cannot read ${layout.manifest}: ${error.message}`);
      continue;
    }
    let found;
    try {
      found = identify(dir, kind, channel, target);
    } catch (error) {
      problems.push(`${kind}: cannot hash ${fileFor(layout, target)}: ${error.message}`);
      continue;
    }
    // The digest first: it is the one the desktop enforces, and a mismatch
    // here explains every other field that disagrees below it.
    if (manifest.sha256 !== found.sha256) {
      problems.push(`${kind}: manifest sha256 ${manifest.sha256} but ${fileFor(layout, target)} hashes to ${found.sha256} (prepare manifests AFTER signing)`);
    }
    if (manifest.build !== found.build) problems.push(`${kind}: manifest build ${manifest.build} but the file's is ${found.build}`);
    if (manifest.size !== found.bytes.length) problems.push(`${kind}: manifest size ${manifest.size} but ${fileFor(layout, target)} is ${found.bytes.length} bytes`);
    if (manifest.url !== found.url) problems.push(`${kind}: manifest url ${manifest.url} is not the ${channel} channel's ${target} ${found.url}`);
  }
  return problems;
}

// Ask the built engine, not source: the desktop pins clients to this version.
// Isolate a build lacking the flag, which might otherwise start in the checkout.
function engineMinecraftVersion(file) {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "pumpkin-version-"));
  let out;
  try {
    out = execFileSync(file, ["--minecraft-version"], {
      cwd,
      encoding: "utf8",
      timeout: 30_000,
      stdio: ["ignore", "pipe", "inherit"],
    });
  } catch (error) {
    console.error(
      `\nCould not ask ${path.relative(ROOT, file)} for its Minecraft version: ${error.message}\n` +
        "  It has to run here, so publish from a machine of its target, and from a build that has\n" +
        "  the --minecraft-version flag.\n"
    );
    process.exit(1);
  } finally {
    fs.rmSync(cwd, { recursive: true, force: true });
  }
  let parsed;
  try {
    parsed = JSON.parse(out.trim().split(/\r?\n/).pop());
  } catch {
    parsed = null;
  }
  const minecraftVersion = parsed?.minecraftVersion;
  const protocol = parsed?.protocol;
  if (
    typeof minecraftVersion !== "string" ||
    !/^\d+(\.\d+){1,2}$/.test(minecraftVersion) ||
    !Number.isInteger(protocol) ||
    protocol <= 0
  ) {
    console.error(`\nThe engine answered --minecraft-version with something else:\n  ${out.trim()}\n`);
    process.exit(1);
  }
  return { minecraftVersion, protocol };
}

// Ask the built runner, not source, for the same reason as above: the manifest
// is what a desktop reads to decide whether this build understands a
// descriptor's runtime layout, and a manifest that says so about a binary that
// does not is worse than one that says nothing. A build too old to have the
// flag publishes no features, which is the truthful answer about it.
function runnerFeatures(file) {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "game-runner-features-"));
  let out;
  try {
    out = execFileSync(file, ["--features"], {
      cwd,
      encoding: "utf8",
      timeout: 30_000,
      stdio: ["ignore", "pipe", "inherit"],
    });
  } catch (error) {
    console.error(
      `\nCould not ask ${path.relative(ROOT, file)} which features it has: ${error.message}\n` +
        "  It has to run here, so publish from a machine of its target, and from a build that has\n" +
        "  the --features flag.\n"
    );
    process.exit(1);
  } finally {
    fs.rmSync(cwd, { recursive: true, force: true });
  }
  let parsed;
  try {
    parsed = JSON.parse(out.trim().split(/\r?\n/).pop());
  } catch {
    parsed = null;
  }
  // A token, or a namespaced one: `extension:<name>` is how a runner says
  // which game extensions are compiled in (homerun-core's extension_feature).
  // The colon is the only punctuation it adds, so the rule still admits
  // nothing that is unsafe in a log line or a comparison.
  const NAME = /^[a-z0-9][a-z0-9-]{0,63}(:[a-z0-9][a-z0-9-]{0,63})?$/;
  if (!Array.isArray(parsed) || parsed.some((f) => typeof f !== "string" || !NAME.test(f))) {
    console.error(`\nThe runner answered --features with something else:\n  ${out.trim()}\n`);
    process.exit(1);
  }
  return parsed;
}

/** The fork revision the wireproxy build script pins (and refuses to build past). */
function wireproxyRev() {
  return fs.readFileSync(path.join(ROOT, "scripts", "wireproxy.rev"), "utf8").trim();
}

/**
 * What to publish when `--only` does not say. Windows keeps what its release
 * job has always built: Pumpkin and the addon, plus a runner already built.
 * Elsewhere, whichever of the target's artifacts have been built.
 */
function defaultSelection(dir, target) {
  if (isLegacyTarget(target)) {
    const selected = ["pumpkin", "core-node"];
    if (fs.existsSync(path.join(dir, LAYOUT["game-runner"].file))) selected.push("game-runner");
    return selected;
  }
  const built = TARGET_KINDS[target].filter((kind) => fs.existsSync(path.join(dir, fileFor(LAYOUT[kind], target))));
  if (!built.length) {
    throw new Error(`Nothing built for ${target} in ${path.relative(ROOT, dir)}. Expected any of: ${TARGET_KINDS[target].join(", ")}.`);
  }
  return built;
}

function main(args, env = {}) {
  const options = parseArgs(args, env);
  const dir = artifactDir(options.target);
  const selected = options.only ? [options.only] : defaultSelection(dir, options.target);

  if (options.verify) {
    const problems = verifyArtifacts(dir, selected, options.channel, options.target);
    if (problems.length) {
      throw new Error(`Manifests do not describe the files beside them:\n  ${problems.join("\n  ")}`);
    }
    const which = isLegacyTarget(options.target) ? options.channel : `${options.channel} ${options.target}`;
    console.log(`Verified ${selected.join(", ")} against their ${which} manifests.`);
    return;
  }

  const metadata = {};
  for (const kind of selected) {
    if (kind === "core-node") {
      // Asked of the addon itself, so it has to load here.
      const host = `${process.platform}-${process.arch}`;
      if (!isLegacyTarget(options.target) && host !== options.target) {
        throw new Error(`The core addon for ${options.target} has to be published from a ${options.target} machine (this is ${host}): its version and ABI are asked of the addon.`);
      }
      const addon = require(path.join(dir, LAYOUT[kind].file));
      metadata[kind] = { version: addon.coreVersion(), abi: addon.coreAbiVersion(), sourceBuild: addon.coreBuildId() };
    } else if (kind === "wireproxy") {
      const rev = wireproxyRev();
      metadata[kind] = { version: rev.slice(0, 12), rev };
    } else {
      // Asked of the binary itself as well, so it has to run here.
      const host = `${process.platform}-${process.arch}`;
      if (!isLegacyTarget(options.target) && host !== options.target) {
        throw new Error(`The ${kind} for ${options.target} has to be published from a ${options.target} machine (this is ${host}): what it can do is asked of the binary.`);
      }
      const crate = kind === "pumpkin" ? "homerun-pumpkin-bin" : "homerun-game-cli";
      const cargo = fs.readFileSync(path.join(ROOT, "rust", crate, "Cargo.toml"), "utf8");
      metadata[kind] = kind === "pumpkin"
        ? { rev: cargo.match(/rev\s*=\s*"([0-9a-f]{7,40})"/)?.[1], ...engineMinecraftVersion(path.join(dir, fileFor(LAYOUT.pumpkin, options.target))) }
        : {
            version: cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1],
            protocol: 1,
            features: runnerFeatures(path.join(dir, fileFor(LAYOUT["game-runner"], options.target))),
          };
    }
  }
  const artifacts = prepareArtifacts(dir, selected, metadata, options.channel, options.target);
  const base = CHANNELS[options.channel].s3;
  const which = isLegacyTarget(options.target) ? options.channel : `${options.channel}, target ${options.target}`;
  console.log(`Channel ${which}. Everything below is under ${base}.\n`);
  console.log("Manifests prepared. Sign artifacts BEFORE this step; regenerate after any byte changes.\n");
  for (const artifact of artifacts) {
    console.log(`${artifact.kind}: ${artifact.manifest.build}, ${artifact.manifest.size} bytes, SHA-256 ${artifact.manifest.sha256}`);
  }
  console.log("\nUpload all immutable binaries before updating any latest.json:");
  for (const artifact of artifacts) console.log(`aws s3 cp "${artifact.file}" ${base}/${artifact.objectKey}`);
  for (const artifact of artifacts) console.log(`aws s3 cp "${artifact.manifestPath}" ${base}/${artifact.manifestKey} --cache-control no-cache`);
  // Released desktops fetch this name; only Windows has released desktops.
  const addon = isLegacyTarget(options.target) && artifacts.find((artifact) => artifact.kind === "core-node");
  if (addon) {
    console.log("\nCompatibility alias for existing desktop builds (new builds should pin the manifest SHA-256):");
    console.log(`aws s3 cp "${addon.file}" ${base}/assets/homerun_core.node`);
  }
}

if (require.main === module) {
  try { main(process.argv.slice(2), process.env || {}); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
module.exports = {
  prepareArtifacts, verifyArtifacts, resolveChannel, resolveTarget, artifactDir, fileFor,
  LAYOUT, CHANNELS, DEFAULT_CHANNEL, CHANNEL_ENV, TARGET_KINDS, DEFAULT_TARGET,
};
