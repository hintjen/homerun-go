#!/usr/bin/env node
/** Prepare manifests from final, signed bytes. Print upload commands; never upload. */
const { execFileSync } = require("child_process");
const os = require("os");
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const { ROOT } = require("./targets");

const S3_BASE = "s3://fractal-homerun/homerun-desktop";
const PUBLIC_BASE = "https://fractal-homerun.s3.amazonaws.com/homerun-desktop";
const LAYOUT = {
  pumpkin: { file: "homerun-desktop-minecraft-pumpkin.exe", prefix: "pumpkin", stem: "homerun-desktop-minecraft-pumpkin", ext: "exe", manifest: "pumpkin-latest.json" },
  "game-runner": { file: "homerun-game.exe", prefix: "game-runner", stem: "homerun-game", ext: "exe", manifest: "game-runner-latest.json" },
  "core-node": { file: "homerun_core.node", prefix: "core", stem: "homerun-core", ext: "node", manifest: "core-latest.json" },
};

function prepareArtifacts(dir, selected, metadata = {}) {
  // Validate every input before replacing any existing manifest.
  const artifacts = selected.map((kind) => {
    const layout = LAYOUT[kind];
    if (!layout) throw new Error(`Unknown artifact: ${kind}`);
    const file = path.join(dir, layout.file);
    const bytes = fs.readFileSync(file);
    if (!bytes.length) throw new Error(`Empty artifact: ${file}`);
    const info = metadata[kind] || {};
    if (kind === "pumpkin" && (!/^\d+(\.\d+){1,2}$/.test(info.minecraftVersion || "") || !Number.isInteger(info.protocol) || info.protocol <= 0)) {
      throw new Error("Pumpkin must identify its Minecraft version and protocol");
    }
    if (kind !== "pumpkin" && !info.version) throw new Error(`Missing version for ${kind}`);
    if (kind === "core-node" && (!Number.isInteger(info.abi) || info.abi < 1 || !info.sourceBuild)) {
      throw new Error("Core addon must export its ABI and source build identity");
    }
    const sha256 = crypto.createHash("sha256").update(bytes).digest("hex");
    const build = sha256.slice(0, 12);
    const objectKey = `${layout.prefix}/${layout.stem}-${build}.${layout.ext}`;
    return {
      kind, file, objectKey, manifestPath: path.join(dir, layout.manifest),
      manifestKey: `${layout.prefix}/latest.json`,
      manifest: { ...info, build, url: `${PUBLIC_BASE}/${objectKey}`, sha256, size: bytes.length },
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

function main(args) {
  const dir = path.join(ROOT, "dist", "desktop");
  // Existing release jobs only build Pumpkin and the addon. A runner is
  // required only when explicitly selected, or included when already built.
  let selected = ["pumpkin", "core-node"];
  if (fs.existsSync(path.join(dir, LAYOUT["game-runner"].file))) selected.push("game-runner");
  if (args.length) {
    if (args.length !== 2 || args[0] !== "--only" || !Object.hasOwn(LAYOUT, args[1])) {
      throw new Error("Usage: node scripts/publish-desktop-artifacts.js [--only pumpkin|game-runner|core-node]");
    }
    selected = [args[1]];
  }
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
  const artifacts = prepareArtifacts(dir, selected, metadata);
  console.log("Manifests prepared. Sign artifacts BEFORE this step; regenerate after any byte changes.\n");
  for (const artifact of artifacts) {
    console.log(`${artifact.kind}: ${artifact.manifest.build}, ${artifact.manifest.size} bytes, SHA-256 ${artifact.manifest.sha256}`);
  }
  console.log("\nUpload all immutable binaries before updating any latest.json:");
  for (const artifact of artifacts) console.log(`aws s3 cp "${artifact.file}" ${S3_BASE}/${artifact.objectKey}`);
  for (const artifact of artifacts) console.log(`aws s3 cp "${artifact.manifestPath}" ${S3_BASE}/${artifact.manifestKey} --cache-control no-cache`);
  const addon = artifacts.find((artifact) => artifact.kind === "core-node");
  if (addon) {
    console.log("\nCompatibility alias for existing desktop builds (new builds should pin the manifest SHA-256):");
    console.log(`aws s3 cp "${addon.file}" ${S3_BASE}/assets/homerun_core.node`);
  }
}

if (require.main === module) {
  try { main(process.argv.slice(2)); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
module.exports = { prepareArtifacts, LAYOUT, PUBLIC_BASE };
