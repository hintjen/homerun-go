#!/usr/bin/env node
/**
 * Fails the build when a host's injected capability object has drifted from the
 * contract it is supposed to mirror.
 *
 *   node scripts/check-capabilities.js
 *
 * ## Why only Android
 *
 * iOS cannot drift: `Capabilities.swift` reads `profiles.ios.capabilities`
 * straight out of the vendored `bridge-v1.json` at runtime and `fatalError`s if
 * it is missing, so the injected object *is* the contract. Android transcribes
 * the same thing by hand into `HostCapabilities.ANDROID`, and a hand copy of a
 * fifteen-field record is a copy that falls behind.
 *
 * It already had. `minigames` was in the contract from the beginning and was
 * never added here; it went unnoticed because the UI reads a missing field as
 * `undefined`, `undefined` is falsy, and the contract happened to say `false`.
 * That is the failure mode worth naming: a capability gate does not break when
 * it drifts, it silently takes the wrong branch, and it takes the *right* one
 * often enough to look fine.
 *
 * ## Why a regex rather than parsing Kotlin
 *
 * The same reason `check-coverage.js` reads the routers with one: this has to
 * run on a CI box with node and nothing else — no Gradle, no JDK, no Android
 * SDK. A parser for one `data class` and one constructor call is not worth a
 * toolchain, and the failure mode of the regex is "reads nothing and fails",
 * which is the safe direction.
 *
 * Capabilities are deliberately outside the revision ledger
 * (`scripts/check-host-revision.js`) — they are self-describing at runtime, so
 * an old host reporting `backups: false` is telling the truth. This check is
 * about the host telling the truth in the first place.
 */
const fs = require("fs");
const path = require("path");

const { ROOT } = require("./targets");

const MANIFEST = path.join(ROOT, "shared", "conformance", "bridge-v1.json");

const HOSTS = [
  {
    profile: "android",
    label: "Android (HostCapabilities.kt)",
    file: path.join(
      ROOT, "android", "app", "src", "main", "java", "app", "gethomerun",
      "mobile", "HostCapabilities.kt"
    ),
    /** The constructor call the activity injects, not the class declaration. */
    constant: /val\s+ANDROID\s*=\s*HostCapabilities\(([\s\S]*?)\n\s*\)/,
  },
];

function fail(message) {
  console.error(`\n${message}\n`);
  process.exit(1);
}

const manifest = JSON.parse(fs.readFileSync(MANIFEST, "utf8"));

let failed = false;

for (const { profile, label, file, constant } of HOSTS) {
  const expected = manifest.profiles?.[profile]?.capabilities;
  if (!expected) {
    fail(
      `${path.relative(ROOT, MANIFEST)} has no profiles.${profile}.capabilities.\n` +
        "Re-run `npm run sync-contract ../homerun-app-ui`."
    );
  }

  let text;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch (err) {
    fail(`Could not read the capabilities for ${label}:\n  ${err.message}`);
  }

  const block = text.match(constant);
  if (!block) {
    fail(
      `Could not find the capability constant for ${label} in\n  ` +
        `${path.relative(ROOT, file)}\n\n` +
        "The pattern in scripts/check-capabilities.js no longer matches. Fix\n" +
        "the pattern rather than deleting the check — drift here does not\n" +
        "crash, it takes the wrong branch."
    );
  }

  // `name = value,` at the top level of the call. Comments in between are
  // ignored by construction: they carry no `=` in this position.
  const declared = new Map(
    Array.from(block[1].matchAll(/^\s*(\w+)\s*=\s*(.+?),\s*$/gm)).map((m) => [
      m[1],
      m[2].trim(),
    ])
  );

  const missing = Object.keys(expected).filter((k) => !declared.has(k));
  const extra = [...declared.keys()].filter((k) => !(k in expected));

  // Only booleans are compared by value. `platform` and `serverBackends` are
  // Kotlin expressions (`listOf("javaNative", …)`) whose text will never equal
  // the JSON, and they are not flags anyone gates on.
  const wrong = [];
  for (const [key, value] of Object.entries(expected)) {
    if (typeof value !== "boolean") continue;
    const kotlin = declared.get(key);
    if (kotlin === undefined) continue;
    if (kotlin !== String(value)) wrong.push({ key, kotlin, value });
  }

  if (!missing.length && !extra.length && !wrong.length) {
    console.log(`  ok    ${label} matches the contract, ${declared.size} fields`);
    continue;
  }

  failed = true;
  console.error(`  FAIL  ${label}`);
  console.error(`        ${path.relative(ROOT, file)}`);

  if (missing.length) {
    console.error(
      `\n        In the contract, missing here (${missing.length}):\n` +
        missing.map((k) => `          ${k} = ${expected[k]}`).join("\n") +
        "\n\n        Add them to the data class and to ANDROID. The UI reads a\n" +
        "        missing field as undefined and gates on it silently."
    );
  }
  if (extra.length) {
    console.error(
      `\n        Declared here, not in the contract (${extra.length}):\n` +
        extra.map((k) => `          ${k}`).join("\n") +
        "\n\n        Either the contract needs re-syncing or this field is dead."
    );
  }
  if (wrong.length) {
    console.error(
      `\n        Present, but disagreeing (${wrong.length}):\n` +
        wrong
          .map((w) => `          ${w.key}: host says ${w.kotlin}, contract says ${w.value}`)
          .join("\n") +
        "\n\n        The contract is generated from the UI's own profile, so it\n" +
        "        wins unless the UI is the thing that is wrong."
    );
  }
  console.error("");
}

if (failed) {
  console.error("\nHost capabilities do not match the contract.\n");
  process.exit(1);
}
console.log("\nPASS — every host injects the capabilities the contract declares.");

// ---------------------------------------------------------------------------
// The bundle public key
// ---------------------------------------------------------------------------

/**
 * Not a capability, and it rides here for one reason: this is already the gate
 * that catches a constant both hosts transcribe by hand, and it already runs in
 * `npm test` on a box with node and nothing else.
 *
 * The key gained a third copy when `build-ui.js` learned to verify a manifest
 * it fetched from the CDN (`plans/repo-split.md` § 3a). A wrong copy in a host
 * does not error — it rejects every manifest for ever, which reads exactly like
 * "nothing has been published". Nothing else would notice.
 */
const { BUNDLE_PUBLIC_KEY, hostDisagreements } = require("./bundle-key");

const keyProblems = hostDisagreements();
if (keyProblems.length) {
  console.error(
    `\nThe bundle public key has drifted from scripts/bundle-key.js:\n\n  ` +
      keyProblems.join("\n  ") +
      "\n\n  A host with the wrong key rejects every over-the-air manifest\n" +
      "  silently. Change all three together, and remember the private half\n" +
      "  in HOMERUN_BUNDLE_KEY has to match.\n"
  );
  process.exit(1);
}
console.log(`PASS — both hosts carry bundle public key ${BUNDLE_PUBLIC_KEY.slice(0, 12)}…`);
