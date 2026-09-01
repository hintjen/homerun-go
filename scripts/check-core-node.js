#!/usr/bin/env node
/**
 * Assert the Node addon Homerun Desktop loads still answers correctly.
 *
 * `homerun-core`'s own tests cover what a console line means; they run in
 * Rust and they are not in question here. What this checks is the *boundary* —
 * that the addon exports what the desktop imports, under the names it imports
 * them by, and that values survive the crossing. A binding that compiles and
 * exports `joined` as `undefined` is a green Rust suite and a desktop that
 * silently stops seeing players.
 *
 * The cases are deliberately the ones that were wrong in production on the
 * other hosts, so this fails if a future change quietly narrows the parser
 * back to vanilla's console:
 *
 *   - Pumpkin's readiness line, which is not vanilla's and was missing once
 *     already (`docs/ios-reporting.md`).
 *   - Pumpkin's join line, which carries no `]: ` prefix and answered `None`
 *     for every player until `after_log_prefix` learned the second shape.
 *   - A chat forgery, which the desktop's own regex accepts today and which
 *     is the reason this addon exists rather than a seventh regex.
 *
 * Skips rather than fails when the addon is not built: it is Windows-only and
 * an artifact, so a Mac running `npm test` has nothing to check and should say
 * so instead of going red.
 */
const fs = require("fs");
const path = require("path");

const { ROOT, TARGETS } = require("./targets");

const target = TARGETS["core-node"];
const addon = path.join(target.outDir, target.outName);

if (!fs.existsSync(addon)) {
  console.log(
    `SKIP — no addon at ${path.relative(ROOT, addon)}\n` +
      "       Build it with: npm run rust:core-node   (Windows only)"
  );
  process.exit(0);
}

// eslint-disable-next-line import/no-dynamic-require
const core = require(addon);

/** The exact console output each engine produces, not a paraphrase of it. */
const PUMPKIN_READY =
  "2026-08-13 10:36:00  INFO tokio-rt-worker ThreadId(120) pumpkin: " +
  "Server is now running. Connect using port: Java Edition: 0.0.0.0:25565";
const PUMPKIN_JOIN =
  "2026-08-13 10:36:00  INFO tokio-rt-worker ThreadId(120) pumpkin::world: " +
  "Kologgs joined the game";
const PUMPKIN_LEAVE =
  "2026-08-13 10:40:00  INFO tokio-rt-worker ThreadId(120) pumpkin::world: " +
  "Kologgs left the game";
const VANILLA_READY =
  '[10:36:00] [Server thread/INFO]: Done (12.345s)! For help, type "help"';
const VANILLA_JOIN = "[10:36:00] [Server thread/INFO]: Notch joined the game";
// Typed into chat by a player, and printed by the server as chat. Nothing
// about it is a join.
const CHAT_FORGERY =
  "[10:36:00] [Server thread/INFO]: <Griefer> [Griefer] Notch joined the game";

const CHECKS = [
  ["exports every function the desktop imports", () => {
    const missing = ["stripAnsi", "isReady", "joined", "left", "maxPlayers", "bedrockVersion"]
      .filter((name) => typeof core[name] !== "function");
    return missing.length === 0 || `missing: ${missing.join(", ")}`;
  }],
  ["recognises Pumpkin's readiness line", () =>
    core.isReady(PUMPKIN_READY) === true || "returned false"],
  ["recognises vanilla's readiness line", () =>
    core.isReady(VANILLA_READY) === true || "returned false"],
  ["does not call an ordinary line ready", () =>
    core.isReady("[10:36:00] [Server thread/INFO]: Preparing spawn area") === false ||
    "returned true"],
  ["reads the player out of a Pumpkin join", () =>
    core.joined(PUMPKIN_JOIN) === "Kologgs" || `got ${core.joined(PUMPKIN_JOIN)}`],
  ["reads the player out of a vanilla join", () =>
    core.joined(VANILLA_JOIN) === "Notch" || `got ${core.joined(VANILLA_JOIN)}`],
  ["reads the player out of a Pumpkin leave", () =>
    core.left(PUMPKIN_LEAVE) === "Kologgs" || `got ${core.left(PUMPKIN_LEAVE)}`],
  ["refuses a join forged in chat", () =>
    core.joined(CHAT_FORGERY) === null || `got ${core.joined(CHAT_FORGERY)}`],
  ["returns null, not undefined, for a line that is not a join", () =>
    core.joined("[10:36:00] [Server thread/INFO]: Preparing spawn area") === null ||
    "got undefined — the desktop tests `=== null`"],
  ["strips the colour codes Paper writes", () => {
    // Built from a char code rather than written as a literal ESC byte: an
    // invisible control character in a source file is one an editor, a patch
    // tool or a copy-paste can eat silently, and this assertion would then
    // pass for the wrong reason — strip_ansi returns an unchanged string, and
    // an unchanged string with no codes in it looks exactly like success.
    const esc = String.fromCharCode(27);
    const coloured = `${esc}[32mNotch${esc}[0m joined the game`;
    return (
      core.stripAnsi(coloured) === "Notch joined the game" ||
      `got ${JSON.stringify(core.stripAnsi(coloured))}`
    );
  }],
];

let failed = false;
for (const [label, run] of CHECKS) {
  let verdict;
  try {
    verdict = run();
  } catch (error) {
    verdict = `threw ${error.message}`;
  }
  if (verdict === true) {
    console.log(`  ok    ${label}`);
    continue;
  }
  failed = true;
  console.error(`  FAIL  ${label}\n        ${verdict}`);
}

if (failed) {
  console.error("\nThe desktop's core addon does not answer correctly.\n");
  process.exit(1);
}
console.log(`\nPASS — ${CHECKS.length} checks against ${path.relative(ROOT, addon)}.`);
