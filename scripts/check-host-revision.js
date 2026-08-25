#!/usr/bin/env node
/**
 * Assert that each host's declared bridge revision matches the channels it
 * actually answers.
 *
 * # Why this number has to exist
 *
 * Today the UI bundle and the host ship in one binary, so a bundle can never
 * call a channel its host does not implement. Over-the-air delivery
 * (`plans/ota-updates.md`) removes that guarantee: a bundle downloaded in July
 * can land on a host built in January. `PROTOCOL.md` §7 versions the protocol
 * (`v: 1`) and says changes are additive — but *additive* is exactly what makes
 * this dangerous, because both hosts still answer `v: 1` and the UI cannot tell
 * them apart.
 *
 * From `CLAUDE.md`:
 *
 * > An unanswered invoke hangs a UI promise forever — that is the worst failure
 * > mode in this protocol, and it looks like a frozen screen with no error.
 *
 * `BRIDGE_HOST_REVISION` is a monotonic integer that says how far along the
 * ledger a host has got. The manifest request carries it, the updater refuses a
 * bundle whose `minHost` exceeds it, and the UI reads it to render a
 * not-yet-implemented feature as absent rather than as a button that hangs.
 *
 * # Why a script, and not a constant somebody remembers to bump
 *
 * A hand-maintained number drifts, and this repo has already watched it happen
 * one layer down: `FFI_ABI_VERSION` went to 2 and `NativeServer.EXPECTED_ABI`
 * stayed at 1, unnoticed, because the only thing that checked ran at runtime in
 * a path nobody was exercising. `check-abi.js` exists because of that.
 *
 * So the check here is not "did you write a number" — it is a comparison
 * against the thing that would make the number wrong. Each host's dispatch
 * table is read the same way `check-coverage.js` reads it, and compared with
 * the ledger:
 *
 *  - a channel answered but never introduced in the ledger  -> FAIL
 *  - a channel introduced above the host's declared revision -> FAIL, bump
 *  - a channel introduced at or below it but not answered    -> FAIL
 *
 * Adding a channel and forgetting to bump is therefore a build failure with the
 * fix named in it, rather than a frozen screen on somebody's phone months later.
 *
 * # Scope: channels, not capabilities
 *
 * `plans/ota-updates.md` says "a channel or a capability". Only channels are
 * tracked here, deliberately. Capabilities are already self-describing —
 * `__homerunCapabilities` is injected at document start and an old host reports
 * `backups: false` truthfully, so a bundle gated on a capability degrades
 * correctly with no revision involved. Channels are the ones with no way to ask.
 */
const fs = require("fs");
const path = require("path");

const { ROOT } = require("./targets");

// Shared with `build-ui.js`, which refuses a bundle whose `minHost` is above
// the revision of the checkout it is staging into. One definition, because two
// readers of the same constant is how they end up disagreeing about it.
const { HOSTS } = require("./host-revision");

const LEDGER = path.join(ROOT, "shared", "conformance", "host-revisions.json");

function fail(message) {
  console.error(`\n${message}\n`);
  process.exit(1);
}

function source(file, what) {
  try {
    return fs.readFileSync(file, "utf8");
  } catch (err) {
    fail(`Could not read ${what}:\n  ${file}\n  ${err.message}`);
  }
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

const ledger = JSON.parse(source(LEDGER, "the revision ledger"));
const revisions = ledger.revisions ?? [];

if (!revisions.length) fail(`No revisions in ${path.relative(ROOT, LEDGER)}.`);

// Contiguous from 1. A gap would make "at or below N" ambiguous about whether
// the missing revision was skipped or simply not written down yet.
revisions.forEach((entry, i) => {
  if (entry.revision !== i + 1) {
    fail(
      `Revision ledger is not contiguous: entry ${i} says revision ` +
        `${entry.revision}, expected ${i + 1}.\n` +
        `  ${path.relative(ROOT, LEDGER)}`
    );
  }
});

/** Every channel introduced for `profile` at or below `revision`. */
function introducedBy(profile, revision) {
  const seen = new Map(); // channel -> the revision that introduced it
  for (const entry of revisions) {
    for (const channel of entry.adds?.[profile] ?? []) {
      if (seen.has(channel)) {
        fail(
          `${profile}: "${channel}" is introduced twice — revision ` +
            `${seen.get(channel)} and revision ${entry.revision}.\n` +
            "A channel enters the ledger once, at the revision that added it."
        );
      }
      seen.set(channel, entry.revision);
    }
  }
  return seen;
}

// ---------------------------------------------------------------------------
// Each host against it
// ---------------------------------------------------------------------------

const newest = revisions[revisions.length - 1].revision;
let failed = false;

for (const { profile, label, file, pattern } of HOSTS) {
  const text = source(file, `the router for ${label}`);

  const declared = text.match(pattern);
  if (!declared) {
    fail(
      `Could not find the bridge revision for ${label} in\n  ` +
        `${path.relative(ROOT, file)}\n\n` +
        "The pattern in scripts/check-host-revision.js no longer matches. Fix\n" +
        "the pattern rather than deleting the check — a revision that drifts\n" +
        "unnoticed is what this exists to prevent."
    );
  }
  const revision = Number(declared[1]);

  if (revision > newest) {
    failed = true;
    console.error(
      `  FAIL  ${label} declares revision ${revision}, but the ledger stops at ${newest}.\n` +
        `        Add the entry to ${path.relative(ROOT, LEDGER)} saying what it introduced.`
    );
    continue;
  }

  const block = text.match(/BRIDGE-CHANNELS-BEGIN([\s\S]*?)BRIDGE-CHANNELS-END/);
  if (!block) {
    fail(
      `No BRIDGE-CHANNELS-BEGIN/END block in ${path.relative(ROOT, file)}.\n` +
        "Wrap the router's dispatch table in those markers — check-coverage.js\n" +
        "reads the same block."
    );
  }
  // Declaration position only, exactly as check-coverage.js reads it: a string
  // buried in a handler body is not a handler for anything.
  const answers = new Set(
    Array.from(block[1].matchAll(/"([^"]+)"\s*(?:to\b|:)/g)).map((m) => m[1])
  );

  const known = introducedBy(profile, revision);

  const unledgered = [...answers].filter((c) => !known.has(c));
  const ahead = [...answers].filter((c) => known.get(c) > revision);
  const owed = [...known]
    .filter(([c, at]) => at <= revision && !answers.has(c))
    .map(([c]) => c);

  if (!unledgered.length && !ahead.length && !owed.length) {
    console.log(`  ok    ${label} declares revision ${revision}, ${answers.size} channels`);
    continue;
  }

  failed = true;
  console.error(`  FAIL  ${label} declares revision ${revision}`);
  console.error(`        ${path.relative(ROOT, file)}`);

  if (unledgered.length) {
    console.error(
      `\n        Answered but not in the ledger (${unledgered.length}):\n` +
        unledgered.map((c) => `          ${c}`).join("\n") +
        `\n\n        Add these to a new revision ${newest + 1} entry in\n` +
        `        ${path.relative(ROOT, LEDGER)}, and set this host to ${newest + 1}.\n` +
        "        A bundle that calls one of these on an older host hangs forever."
    );
  }
  if (ahead.length) {
    console.error(
      `\n        Answered, and the ledger introduces them above ${revision} (${ahead.length}):\n` +
        ahead.map((c) => `          ${c}  (revision ${known.get(c)})`).join("\n") +
        `\n\n        Bump this host's revision — it is further along than it says.`
    );
  }
  if (owed.length) {
    console.error(
      `\n        In the ledger at or below ${revision}, but unanswered (${owed.length}):\n` +
        owed.map((c) => `          ${c}  (revision ${known.get(c)})`).join("\n") +
        `\n\n        Implement them, or lower this host's revision to before they\n` +
        "        were introduced. Claiming a revision is a promise the manifest\n" +
        "        server and the updater both act on."
    );
  }
  console.error("");
}

if (failed) {
  console.error("\nBridge host revision mismatch.\n");
  process.exit(1);
}
console.log(`\nPASS — ledger at revision ${newest}; every host matches what it answers.`);
