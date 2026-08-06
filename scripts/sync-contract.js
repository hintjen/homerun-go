#!/usr/bin/env node
/**
 * Refreshes the vendored bridge contract from the homerun-app-ui repo.
 *
 *   node scripts/sync-contract.js [path-to-homerun-app-ui]
 *
 * The manifest and PROTOCOL.md are vendored (not fetched at build time) so
 * mobile CI does not need the UI repo — but they must be re-synced whenever
 * the contract changes. `check-coverage.js` then fails any host that has
 * fallen behind, which is the point: a new required channel should break the
 * mobile build loudly rather than surface as a hung promise on device.
 */
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const uiRepo = path.resolve(
  ROOT,
  process.argv[2] || process.env.HOMERUN_UI_SRC || "../homerun-app-ui"
);

const files = [
  ["lib/bridge/conformance/bridge-v1.json", "shared/conformance/bridge-v1.json"],
  ["lib/bridge/PROTOCOL.md", "shared/conformance/PROTOCOL.md"],
];

if (!fs.existsSync(path.join(uiRepo, "package.json"))) {
  console.error(
    `No homerun-app-ui checkout at ${uiRepo}\n` +
      "Pass the path, or set HOMERUN_UI_SRC."
  );
  process.exit(1);
}

let changed = 0;
for (const [from, to] of files) {
  const src = path.join(uiRepo, from);
  const dst = path.join(ROOT, to);
  const next = fs.readFileSync(src, "utf8");
  const prev = fs.existsSync(dst) ? fs.readFileSync(dst, "utf8") : null;
  if (prev === next) {
    console.log(`unchanged  ${to}`);
    continue;
  }
  fs.writeFileSync(dst, next);
  console.log(`updated    ${to}`);
  changed++;
}

if (changed) {
  const m = JSON.parse(
    fs.readFileSync(path.join(ROOT, "shared/conformance/bridge-v1.json"), "utf8")
  );
  console.log(
    `\nbridge/v1 now: ${m.counts.total} channels — ` +
      Object.entries(m.profiles)
        .map(([k, v]) => `${k} ${v.required.length}`)
        .join(", ") +
      "\nRun the coverage check for each host before committing."
  );
}
