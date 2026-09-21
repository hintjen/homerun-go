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

const LAYOUT = {
  pumpkin: { file: "homerun-desktop-minecraft-pumpkin.exe", prefix: "pumpkin", stem: "homerun-desktop-minecraft-pumpkin", ext: "exe", manifest: "pumpkin-latest.json" },
  "game-runner": { file: "homerun-game.exe", prefix: "game-runner", stem: "homerun-game", ext: "exe", manifest: "game-runner-latest.json" },
  "core-node": { file: "homerun_core.node", prefix: "core", stem: "homerun-core", ext: "node", manifest: "core-latest.json" },
};

const USAGE =
  "Usage: node scripts/publish-desktop-artifacts.js " +
  "[--channel dev|prod] [--only pumpkin|game-runner|core-node]";

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
  const options = { only: null, channel: null };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    const key = arg === "--only" ? "only" : arg === "--channel" ? "channel" : null;
    if (!key) throw new Error(USAGE);
    const value = argv[++index];
    if (value === undefined || options[key] !== null) throw new Error(USAGE);
    options[key] = value;
  }
  if (options.only !== null && !Object.hasOwn(LAYOUT, options.only)) throw new Error(USAGE);
  return { ...options, channel: resolveChannel(options.channel, env) };
}

/** What a file's final bytes say about where it is published, and as what. */
function identify(dir, kind, channel) {
  const layout = LAYOUT[kind];
  if (!layout) throw new Error(`Unknown artifact: ${kind}`);
  const file = path.join(dir, layout.file);
  const bytes = fs.readFileSync(file);
  if (!bytes.length) throw new Error(`Empty artifact: ${file}`);
  const sha256 = crypto.createHash("sha256").update(bytes).digest("hex");
  const build = sha256.slice(0, 12);
  const objectKey = `${layout.prefix}/${layout.stem}-${build}.${layout.ext}`;
  return {
    layout, file, bytes, sha256, build, objectKey,
    url: `${CHANNELS[channel].public}/${objectKey}`,
    manifestPath: path.join(dir, layout.manifest),
    manifestKey: `${layout.prefix}/latest.json`,
  };
}

function prepareArtifacts(dir, selected, metadata = {}, channel = DEFAULT_CHANNEL) {
  resolveChannel(channel);
  // Validate every input before replacing any existing manifest.
  const artifacts = selected.map((kind) => {
    const found = identify(dir, kind, channel);
    const info = metadata[kind] || {};
    if (kind === "pumpkin" && (!/^\d+(\.\d+){1,2}$/.test(info.minecraftVersion || "") || !Number.isInteger(info.protocol) || info.protocol <= 0)) {
      throw new Error("Pumpkin must identify its Minecraft version and protocol");
    }
    if (kind !== "pumpkin" && !info.version) throw new Error(`Missing version for ${kind}`);
    if (kind === "core-node" && (!Number.isInteger(info.abi) || info.abi < 1 || !info.sourceBuild)) {
      throw new Error("Core addon must export its ABI and source build identity");
    }
    return {
      kind, channel, file: found.file, objectKey: found.objectKey,
      manifestPath: found.manifestPath, manifestKey: found.manifestKey,
      manifest: { ...info, build: found.build, url: found.url, sha256: found.sha256, size: found.bytes.length },
    };
  });
  for (const artifact of artifacts) {
    fs.writeFileSync(artifact.manifestPath, `${JSON.stringify(artifact.manifest, null, 2)}\n`);
  }
  return artifacts;
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
        "  It has to run here, so publish from Windows, and from a build that has\n" +
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

function main(args, env = {}) {
  const options = parseArgs(args, env);
  const dir = path.join(ROOT, "dist", "desktop");
  // Existing release jobs only build Pumpkin and the addon. A runner is
  // required only when explicitly selected, or included when already built.
  let selected = ["pumpkin", "core-node"];
  if (fs.existsSync(path.join(dir, LAYOUT["game-runner"].file))) selected.push("game-runner");
  if (options.only) selected = [options.only];

  const metadata = {};
  for (const kind of selected) {
    if (kind === "core-node") {
      const addon = require(path.join(dir, LAYOUT[kind].file));
      metadata[kind] = { version: addon.coreVersion(), abi: addon.coreAbiVersion(), sourceBuild: addon.coreBuildId() };
    } else {
      const crate = kind === "pumpkin" ? "homerun-pumpkin-bin" : "homerun-game-cli";
      const cargo = fs.readFileSync(path.join(ROOT, "rust", crate, "Cargo.toml"), "utf8");
      metadata[kind] = kind === "pumpkin"
        ? { rev: cargo.match(/rev\s*=\s*"([0-9a-f]{7,40})"/)?.[1], ...engineMinecraftVersion(path.join(dir, LAYOUT.pumpkin.file)) }
        : { version: cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1], protocol: 1 };
    }
  }
  const artifacts = prepareArtifacts(dir, selected, metadata, options.channel);
  const base = CHANNELS[options.channel].s3;
  console.log(`Channel ${options.channel}. Everything below is under ${base}.\n`);
  console.log("Manifests prepared. Sign artifacts BEFORE this step; regenerate after any byte changes.\n");
  for (const artifact of artifacts) {
    console.log(`${artifact.kind}: ${artifact.manifest.build}, ${artifact.manifest.size} bytes, SHA-256 ${artifact.manifest.sha256}`);
  }
  console.log("\nUpload all immutable binaries before updating any latest.json:");
  for (const artifact of artifacts) console.log(`aws s3 cp "${artifact.file}" ${base}/${artifact.objectKey}`);
  for (const artifact of artifacts) console.log(`aws s3 cp "${artifact.manifestPath}" ${base}/${artifact.manifestKey} --cache-control no-cache`);
  const addon = artifacts.find((artifact) => artifact.kind === "core-node");
  if (addon) {
    console.log("\nCompatibility alias for existing desktop builds (new builds should pin the manifest SHA-256):");
    console.log(`aws s3 cp "${addon.file}" ${base}/assets/homerun_core.node`);
  }
}

if (require.main === module) {
  try { main(process.argv.slice(2), process.env || {}); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
module.exports = { prepareArtifacts, resolveChannel, LAYOUT, CHANNELS, DEFAULT_CHANNEL, CHANNEL_ENV };
