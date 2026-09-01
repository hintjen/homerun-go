#!/usr/bin/env node
/**
 * Prepare the artifacts Homerun Desktop downloads, and the manifest it reads.
 *
 * The desktop packages neither of these. The Pumpkin engine is fetched at
 * launch — the same arrangement Bedrock Dedicated Server has had all along —
 * so a new engine reaches players without a desktop release, and ~114 MB stays
 * out of the installer for everyone who never makes a Pumpkin server. The Node
 * addon is fetched at build time by `download-assets.js`, because it is code
 * the main process loads at startup rather than a server it spawns.
 *
 * # Why the build id is the digest
 *
 * Pumpkin publishes no version number, and inventing one here would be a second
 * source of truth about which engine a world has met. The digest cannot
 * disagree with the file: rebuild the same source and the id is unchanged, so
 * nobody re-downloads 114 MB to arrive where they already were. The fork
 * revision rides along as `rev` for people, not for comparison.
 *
 * # What this does not do
 *
 * It does not upload. The digests and the manifest are computed here and the
 * exact commands are printed, so the credentials stay in CI where they belong
 * and a local run cannot publish by accident.
 *
 * Usage:
 *   npm run rust:pumpkin-bin-windows && npm run rust:core-node
 *   node scripts/publish-desktop-artifacts.js
 */
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

const { ROOT, TARGETS } = require("./targets");

/** Where CI puts them. Mirrored in the desktop's PUMPKIN_MANIFEST_URL. */
const S3_BUCKET = "s3://fractal-homerun/homerun-desktop";
const PUBLIC_BASE = "https://fractal-homerun.s3.amazonaws.com/homerun-desktop";

const DIST = path.join(ROOT, "dist", "desktop");

function sha256(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function requireArtifact(targetName) {
  const target = TARGETS[targetName];
  const file = path.join(target.outDir, target.outName || target.artifact);
  if (!fs.existsSync(file)) {
    console.error(
      `\nMissing ${path.relative(ROOT, file)}\n` +
        `  Build it first: npm run rust:${targetName}\n`
    );
    process.exit(1);
  }
  return file;
}

/** The fork revision the engine was built from, for the manifest's `rev`. */
function pumpkinRev() {
  const cargo = fs.readFileSync(
    path.join(ROOT, "rust", "homerun-pumpkin-bin", "Cargo.toml"),
    "utf8"
  );
  const match = cargo.match(/rev\s*=\s*"([0-9a-f]{7,40})"/);
  return match ? match[1] : null;
}

const engine = requireArtifact("pumpkin-bin-windows");
const addon = requireArtifact("core-node");

const engineDigest = sha256(engine);
const engineSize = fs.statSync(engine).size;
// Twelve hex characters: enough that a collision is not a thing that happens,
// short enough to be a directory name a person can read in a log line.
const build = engineDigest.slice(0, 12);
const engineName = `homerun-desktop-minecraft-pumpkin-${build}.exe`;

const manifest = {
  build,
  url: `${PUBLIC_BASE}/pumpkin/${engineName}`,
  sha256: engineDigest,
  size: engineSize,
  rev: pumpkinRev() || undefined,
};

const manifestPath = path.join(DIST, "pumpkin-latest.json");
fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

const addonDigest = sha256(addon);

console.log(`\nPumpkin engine   ${path.relative(ROOT, engine)}`);
console.log(`  build          ${build}`);
console.log(`  sha256         ${engineDigest}`);
console.log(`  size           ${(engineSize / 1024 / 1024).toFixed(1)} MB`);
console.log(`  rev            ${manifest.rev ?? "(unknown)"}`);
console.log(`\nNode addon       ${path.relative(ROOT, addon)}`);
console.log(`  sha256         ${addonDigest}`);
console.log(`\nManifest         ${path.relative(ROOT, manifestPath)}`);

console.log(`
To publish (CI, or a maintainer with credentials):

  # The engine first. The manifest must never name a file that is not there
  # yet, or a launch between the two uploads downloads a 404.
  aws s3 cp "${engine}" ${S3_BUCKET}/pumpkin/${engineName}
  aws s3 cp "${manifestPath}" ${S3_BUCKET}/pumpkin/latest.json --cache-control no-cache

  # The addon is pulled at desktop build time, not at launch, so it has no
  # manifest and is simply replaced.
  aws s3 cp "${addon}" ${S3_BUCKET}/assets/homerun_core.node

Sign both before uploading if the signing account is available: they arrive on
a player's disk unannounced and are executed, and unlike BDS and the Zulu JREs
they carry nobody else's signature.
`);
