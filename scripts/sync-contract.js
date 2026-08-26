#!/usr/bin/env node
/**
 * Refreshes the vendored channel manifest from the private UI repo.
 *
 *   node scripts/sync-contract.js [path-to-ui-repo]
 *
 * `bridge-v1.json` is generated there from `channels.ts` and
 * `requirements.ts`, so it can only flow in this direction. It is vendored
 * (not fetched at build time) so mobile CI needs no UI checkout — but it must
 * be re-synced whenever a channel changes. `check-coverage.js` then fails any
 * host that has fallen behind, which is the point: a new required channel
 * should break the mobile build loudly rather than surface as a hung promise
 * on device.
 *
 * # PROTOCOL.md is NOT synced, deliberately
 *
 * It used to be copied here alongside the manifest, and it rotted: the copy
 * still said the WebView hosts were coming "next" long after both had shipped
 * and passed conformance. Nothing caught that, because no gate reads the
 * prose — `check-coverage.js` and `check-capabilities.js` parse the manifest,
 * and PROTOCOL.md appears only in a comment and an error string.
 *
 * So it is canonical *here* now. That is where its change-drivers live: §3.2
 * and §3.3 are this repo's iOS and Android transports, and §4 is host
 * lifecycle. The manifest churns when a channel is added; the prose churns
 * when a host does. Adding it back to `files` below would silently revert
 * whatever this repo has written.
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
];

if (!fs.existsSync(path.join(uiRepo, "package.json"))) {
  console.error(
    `No UI checkout at ${uiRepo}\n` +
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
