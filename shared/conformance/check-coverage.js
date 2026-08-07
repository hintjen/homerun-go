#!/usr/bin/env node
/**
 * Fails a host's build when it does not implement every channel its declared
 * profile requires.
 *
 *   node shared/conformance/check-coverage.js ios     ios/HomerunHost/BridgeRouter.swift
 *   node shared/conformance/check-coverage.js android android/app/src/main/java/.../BridgeRouter.kt
 *
 * Hosts declare coverage by listing channel strings between the markers
 *
 *   // BRIDGE-CHANNELS-BEGIN
 *   ... "get-app-version", "native-server-start", ...
 *   // BRIDGE-CHANNELS-END
 *
 * so this works identically for Swift and Kotlin without parsing either
 * language. The block is the host's own dispatch table, not a duplicate
 * list — keep them the same thing.
 *
 * Events are excluded: a host *emits* those, and emitting nothing is a
 * runtime concern the manifest cannot prove. Invoke and send channels are
 * what a router must answer.
 */
const fs = require("fs");
const path = require("path");

const [, , profileName, sourcePath] = process.argv;
if (!profileName || !sourcePath) {
  console.error(
    "usage: check-coverage.js <ios|android|desktop> <path-to-router-source>"
  );
  process.exit(2);
}

const manifest = JSON.parse(
  fs.readFileSync(path.join(__dirname, "bridge-v1.json"), "utf8")
);

const profile = manifest.profiles[profileName];
if (!profile) {
  console.error(
    `Unknown profile "${profileName}". Known: ${Object.keys(manifest.profiles).join(", ")}`
  );
  process.exit(2);
}

// Only invoke/send need a handler; events flow the other way.
const answerable = new Set(
  manifest.channels
    .filter((c) => c.kind === "invoke" || c.kind === "send")
    .map((c) => c.channel)
);
const required = profile.required.filter((c) => answerable.has(c));

const source = fs.readFileSync(sourcePath, "utf8");
const block = source.match(
  /BRIDGE-CHANNELS-BEGIN([\s\S]*?)BRIDGE-CHANNELS-END/
);
if (!block) {
  console.error(
    `No BRIDGE-CHANNELS-BEGIN/END block found in ${sourcePath}.\n` +
      "Wrap the router's dispatch table in those markers."
  );
  process.exit(1);
}

// Only strings in *declaration position* count — `"channel" to handler`
// (Kotlin), `"channel": handler` or `case "channel":` (Swift). The block is
// the dispatch table itself, so it contains handler bodies too, and matching
// every quoted string in it would let a literal buried in a body pass as a
// handler for a channel nobody implemented. Keep the table in one of those
// two forms; a router that registers channels some other way will read as
// having no handlers at all, which is the safe direction to fail.
const declared = new Set(
  Array.from(block[1].matchAll(/"([^"]+)"\s*(?:to\b|:)/g)).map((m) => m[1])
);

const missing = required.filter((c) => !declared.has(c));
const unknown = Array.from(declared).filter((c) => !answerable.has(c));

const byChannel = new Map(manifest.channels.map((c) => [c.channel, c]));

console.log(
  `\nbridge/v1 coverage — profile "${profileName}" (${sourcePath})\n` +
    `  required ${required.length}   declared ${declared.size}   missing ${missing.length}\n`
);

if (missing.length) {
  console.error("Missing handlers:\n");
  for (const c of missing) {
    const meta = byChannel.get(c);
    console.error(`  ${c}${meta?.note ? `\n      ${meta.note}` : ""}`);
  }
  console.error("");
}

if (unknown.length) {
  // Not fatal: a host may answer desktop-only channels benignly, and the
  // manifest marks some of those `ungated` precisely because the calling UI
  // is not capability-gated yet.
  console.log("Declared but not required by this profile (fine, just noting):");
  for (const c of unknown) console.log(`  ${c}`);
  console.log("");
}

if (missing.length) {
  console.error(
    `FAIL — ${missing.length} required channel(s) unhandled. ` +
      "An unhandled invoke hangs a UI promise forever (PROTOCOL.md §5)."
  );
  process.exit(1);
}

console.log("PASS — every required channel has a handler.");
