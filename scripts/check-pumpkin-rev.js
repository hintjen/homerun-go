#!/usr/bin/env node
/**
 * Assert that every crate depending on Pumpkin pins the *same* rev.
 *
 * Two crates now name the fork independently: `homerun-pumpkin-ffi`, which
 * links it for iOS and — more to the point here — writes the `pumpkin.toml`
 * that configures a run, and `homerun-pumpkin-bin`, which *is* the server
 * Android starts. They are two halves of one thing: the FFI crate spells the
 * config keys, the binary reads them.
 *
 * So a drift between them is not a version skew, it is a silent
 * misconfiguration. Upstream renames a settings key, one crate moves and the
 * other does not, and the host writes a file the running server ignores —
 * which looks exactly like a server that started on its own defaults, because
 * that is what it is. `online_mode` alone makes that the difference between a
 * joinable server and an unjoinable one.
 *
 * The rev is also the Minecraft protocol version. Two revs would mean the
 * world format the app expects and the one the server writes could differ,
 * and that is a save-eating class of bug rather than a configuration one.
 *
 * Same idea as `check-abi.js`: read the source rather than trust that two
 * people remembered to edit both files.
 */
const fs = require("fs");
const path = require("path");

const { ROOT } = require("./targets");

/** Every manifest that may name the fork. */
const MANIFESTS = [
  path.join(ROOT, "rust", "homerun-pumpkin-ffi", "Cargo.toml"),
  path.join(ROOT, "rust", "homerun-pumpkin-bin", "Cargo.toml"),
];

/** `<name> = { git = "…/Pumpkin", rev = "…" }`, however the keys are ordered. */
const DEP = /^\s*([\w-]+)\s*=\s*\{[^}]*?git\s*=\s*"([^"]*Pumpkin[^"]*)"[^}]*?\}/gim;
const REV = /rev\s*=\s*"([0-9a-f]{7,40})"/i;

const found = [];
let missingRev = false;

for (const manifest of MANIFESTS) {
  if (!fs.existsSync(manifest)) continue;
  const source = fs.readFileSync(manifest, "utf8");
  const where = path.relative(ROOT, manifest).replace(/\\/g, "/");

  for (const match of source.matchAll(DEP)) {
    const [line, crate] = match;
    const rev = line.match(REV)?.[1];
    if (!rev) {
      // A branch or a floating dep is the thing this check exists to prevent:
      // upstream tracks protocol releases, and that churn must not land in an
      // app build uninvited.
      console.error(`  ${where}: \`${crate}\` names the fork with no \`rev\``);
      missingRev = true;
      continue;
    }
    found.push({ where, crate, rev });
  }
}

if (found.length === 0 && !missingRev) {
  console.error("check-pumpkin-rev: no Pumpkin dependency found in any manifest.");
  console.error("Either the fork was dropped, or this check is looking in the wrong place.");
  process.exit(1);
}

const revs = [...new Set(found.map((f) => f.rev))];

if (missingRev || revs.length > 1) {
  console.error("\nThe Pumpkin rev is not pinned consistently:\n");
  for (const { where, crate, rev } of found) {
    console.error(`  ${rev}  ${where}  (${crate})`);
  }
  console.error(
    "\nEvery crate must name one rev. `homerun-pumpkin-ffi` writes the config\n" +
      "that `homerun-pumpkin-bin` reads, so a split means the host configures a\n" +
      "server that is not the one it started — which presents as a server\n" +
      "running on its own defaults, with nothing to say so.\n",
  );
  process.exit(1);
}

console.log(`OK  Pumpkin pinned at ${revs[0].slice(0, 7)} across ${found.length} dependencies`);
