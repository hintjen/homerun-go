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
 * The bundle cases check the other thing the desktop takes from here: the
 * over-the-air verifier. They start from the manifest `scripts/sign-manifest.js`
 * signed for the pinned vector in `homerun-core`'s `bundle.rs`, not one signed
 * here, so a verifier that agreed only with this script could not pass. Every
 * verdict is reached by varying `installed`, because that is the half the
 * desktop builds itself and the half most likely to be spelled wrong; and a
 * tampered manifest must *throw*, since a refusal that came back as a verdict
 * is one a caller could log and carry on past.
 *
 * Skips rather than fails when the addon is not built: it is Windows-only and
 * an artifact, so a Mac running `npm test` has nothing to check and should say
 * so instead of going red.
 */
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

const { signingPayload } = require("./sign-manifest");
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
if (core.coreAbiVersion?.() !== 1 ||
    !/^\d+\.\d+\.\d+/.test(core.coreVersion?.() || "") ||
    !core.coreBuildId?.()) {
  throw new Error("The addon must identify its ABI, version and source build.");
}

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

/**
 * `JS_SIGNED` and `JS_PUBLIC_KEY`, read out of the tests in
 * `rust/homerun-core/src/bundle.rs` rather than copied here. One copy of the
 * vector means this cannot drift from the one the Rust tests pin — and a
 * second literal of the throwaway key is exactly what secret scanners flag,
 * even though it signs nothing real.
 */
function pinnedVector() {
  const source = fs.readFileSync(path.join(ROOT, "rust", "homerun-core", "src", "bundle.rs"), "utf8");
  const signed = source.match(/const JS_SIGNED: &str = r#"([\s\S]*?)"#;/);
  const key = source.match(/const JS_PUBLIC_KEY: &str = "([0-9a-f]{64})";/);
  if (!signed || !key) {
    console.error("FAIL — could not find JS_SIGNED / JS_PUBLIC_KEY in rust/homerun-core/src/bundle.rs");
    process.exit(1);
  }
  // Compact, so the tampering case below can rewrite one field in place.
  return { manifest: JSON.stringify(JSON.parse(signed[1])), key: key[1] };
}
const { manifest: PINNED_MANIFEST, key: PINNED_KEY } = pinnedVector();

/** What the desktop would say it is serving. Defaults to the shipped copy. */
const installed = (overrides = {}) =>
  JSON.stringify({ bundle: null, serial: 0, hostRevision: 1, platform: "android", ...overrides });

/** The parsed reply. A non-string return is a failure in its own right. */
function evaluate(manifest, key, record) {
  const raw = core.bundleEvaluate(manifest, key, record);
  if (typeof raw !== "string") throw new Error(`returned ${typeof raw}, not a JSON string`);
  return JSON.parse(raw);
}

/** Passes only if `run` throws, with a message containing `fragment`. */
function throwsWith(run, fragment) {
  try {
    run();
  } catch (error) {
    return error.message.includes(fragment) || `threw the wrong thing: ${error.message}`;
  }
  return "did not throw";
}

/** Passes if the reply carries this verdict, `install` agrees, and there is a reason to log. */
function verdictIs(reply, expected) {
  if (reply.verdict.verdict !== expected || reply.install !== (expected === "install")) {
    return `got ${JSON.stringify(reply)}`;
  }
  return (typeof reply.reason === "string" && reply.reason.length > 0) || "no reason for the log";
}

/**
 * A manifest built for the desktop, signed with a key made for this run.
 * There is no pinned vector for `windows` — nothing has been published for it
 * — and what this proves is that the core keeps no list of platforms, which a
 * fresh key proves as well as a pinned one.
 */
function windowsManifest() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync("ed25519");
  const manifest = {
    bundle: "2026-09-14.1",
    url: "https://cdn.gethomerun.app/ui/windows/2026-09-14.1.zip",
    sha256: "0".repeat(64),
    minHost: 1,
    serial: 1,
    platform: "windows",
  };
  manifest.signature = crypto
    .sign(null, Buffer.from(signingPayload(manifest)), privateKey)
    .toString("hex");
  return {
    manifest: JSON.stringify(manifest),
    key: publicKey.export({ type: "spki", format: "der" }).subarray(-32).toString("hex"),
  };
}

const CHECKS = [
  ["exports every function the desktop imports", () => {
    const missing = [
      "stripAnsi", "isReady", "joined", "left", "maxPlayers", "bedrockVersion",
      "bundleEvaluate", "bundleDigestMatches",
    ].filter((name) => typeof core[name] !== "function");
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
  ["verifies the pinned manifest and says install over the shipped copy", () => {
    const reply = evaluate(PINNED_MANIFEST, PINNED_KEY, installed());
    const { manifest } = reply;
    if (manifest.bundle !== "2026-08-14.1" || manifest.serial !== 3 || manifest.minHost !== 1) {
      return `the manifest did not cross intact: ${JSON.stringify(manifest)}`;
    }
    return verdictIs(reply, "install");
  }],
  ["says upToDate for the bundle already being served", () =>
    verdictIs(
      evaluate(PINNED_MANIFEST, PINNED_KEY, installed({ bundle: "2026-08-14.1", serial: 3 })),
      "upToDate"
    )],
  ["says downgrade for a serial below the installed one", () => {
    const reply = evaluate(
      PINNED_MANIFEST, PINNED_KEY, installed({ bundle: "2026-09-01.1", serial: 5 })
    );
    if (reply.verdict.offered !== 3 || reply.verdict.installed !== 5) {
      return `the verdict lost its serials: ${JSON.stringify(reply.verdict)}`;
    }
    return verdictIs(reply, "downgrade");
  }],
  ["says wrongPlatform when a windows host is offered Android's bundle", () => {
    const reply = evaluate(PINNED_MANIFEST, PINNED_KEY, installed({ platform: "windows" }));
    if (reply.verdict.offered !== "android" || reply.verdict.host !== "windows") {
      return `the verdict lost its platforms: ${JSON.stringify(reply.verdict)}`;
    }
    return verdictIs(reply, "wrongPlatform");
  }],
  ["says tooNew for a host below minHost", () => {
    const reply = evaluate(PINNED_MANIFEST, PINNED_KEY, installed({ hostRevision: 0 }));
    if (reply.verdict.required !== 1 || reply.verdict.host !== 0) {
      return `the verdict lost its revisions: ${JSON.stringify(reply.verdict)}`;
    }
    return verdictIs(reply, "tooNew");
  }],
  ["installs a manifest signed for windows on a windows host", () => {
    const { manifest, key } = windowsManifest();
    return verdictIs(evaluate(manifest, key, installed({ platform: "windows" })), "install");
  }],
  ["throws for a manifest whose serial was changed in transit", () => {
    const tampered = PINNED_MANIFEST.replace('"serial":3', '"serial":4');
    if (tampered === PINNED_MANIFEST) return "the fixture did not change, so nothing was tested";
    return throwsWith(
      () => core.bundleEvaluate(tampered, PINNED_KEY, installed()),
      "signature does not match"
    );
  }],
  ["throws for the genuine manifest checked against another key", () =>
    throwsWith(
      () => core.bundleEvaluate(PINNED_MANIFEST, windowsManifest().key, installed()),
      "signature does not match"
    )],
  ["throws for an installed record that is not JSON", () =>
    throwsWith(() => core.bundleEvaluate(PINNED_MANIFEST, PINNED_KEY, "{"), "bad installed record")],
  ["throws for an installed record missing a field", () =>
    throwsWith(
      () => core.bundleEvaluate(PINNED_MANIFEST, PINNED_KEY, '{"bundle":null,"serial":0}'),
      "bad installed record"
    )],
  ["matches a digest regardless of case", () =>
    core.bundleDigestMatches(
      "d2045f55566b0d63ab5ac9216c8b068117a18043f0ba6453f7098dcbf8a4b038",
      "D2045F55566B0D63AB5AC9216C8B068117A18043F0BA6453F7098DCBF8A4B038"
    ) === true || "returned false"],
  ["does not match a different digest", () =>
    core.bundleDigestMatches("a".repeat(64), "c".repeat(64)) === false || "returned true"],
  // The local network: what the desktop binds and what it shouts. The beacon
  // bytes are the same ones Pumpkin's own broadcaster sends and a Java client
  // parses, so the desktop must not spell its own.
  ["a server exposed to the local network binds every interface and says so", () => {
    const bind = JSON.parse(core.lanBind(true, 25565));
    return (bind.address === "0.0.0.0" && /0.0.0.0:25565/.test(bind.line || "")) ||
      JSON.stringify(bind);
  }],
  ["a server not exposed binds loopback and says nothing", () => {
    const bind = JSON.parse(core.lanBind(false, 25565));
    return (bind.address === "127.0.0.1" && bind.line === undefined) || JSON.stringify(bind);
  }],
  ["the LAN beacon is what a Java client lists", () => {
    const beacon = JSON.parse(core.lanBeacon("§aMy\nServer", 25566));
    return (beacon.payload === "[MOTD]My Server[/MOTD][AD]25566[/AD]" &&
      beacon.group === "224.0.2.60" && beacon.port === 4445 && beacon.intervalMs === 1500) ||
      JSON.stringify(beacon);
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
