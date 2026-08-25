#!/usr/bin/env node
/**
 * The Ed25519 public key that verifies an over-the-air bundle manifest, and
 * the check that both hosts still carry the same one.
 *
 *   node scripts/bundle-key.js        # print the key and confirm the hosts agree
 *
 * # Why this file exists
 *
 * The key was written down twice — `ios/HomerunHost/Capabilities.swift` and
 * `android/app/build.gradle.kts` — and `scripts/build-ui.js` now needs a third
 * copy to verify a manifest it fetches from the CDN
 * (`plans/repo-split.md` § 3a). Three hand-copies of one 64-character constant
 * is a typo waiting to happen, and the failure mode is the worst kind: a host
 * with a wrong key does not error, it rejects every manifest for ever, which
 * is indistinguishable from "no releases have been published".
 *
 * The hosts cannot `require` this — one is Kotlin read by Gradle, the other a
 * Swift constant compiled in — so they keep their literals and this file is
 * the authority they are checked against. `check-capabilities.js` runs that
 * check, so `npm test` fails on a drift rather than a device doing it silently.
 *
 * # Changing it
 *
 * A device only accepts manifests signed by the key compiled into *it*, so
 * changing this needs a store release and every installed copy holds the old
 * key until it updates. Change all three together: here, both hosts, and the
 * `HOMERUN_BUNDLE_KEY` secret's private half.
 */
const fs = require("fs");
const path = require("path");

const { ROOT } = require("./targets");

/**
 * Generated 2026-08-13. The private half lives in the `ui-bundle-publish`
 * environment's `HOMERUN_BUNDLE_KEY` secret and nowhere else.
 */
const BUNDLE_PUBLIC_KEY =
  "8d44ecfa010fe0136b450baee986a352cd027d3555403f0662dce5eb2ff16f4e";

/**
 * Where each host writes its own copy.
 *
 * Read with a regex for the same reason `check-coverage.js` and
 * `check-capabilities.js` do: this has to run on a CI box with node and
 * nothing else. The failure mode of a regex here is "matches nothing and
 * fails", which is the safe direction.
 */
const HOSTS = [
  {
    label: "Android (app/build.gradle.kts)",
    file: path.join(ROOT, "android", "app", "build.gradle.kts"),
    // The Gradle default, not a `-PbundlePublicKey` override.
    pattern: /"bundlePublicKey",\s*\n\s*"([0-9a-f]{64})"/,
  },
  {
    label: "iOS (HomerunHost/Capabilities.swift)",
    file: path.join(ROOT, "ios", "HomerunHost", "Capabilities.swift"),
    pattern: /bundlePublicKey\s*=\s*"([0-9a-f]{64})"/,
  },
];

/**
 * Every way the hosts disagree with this file, as sentences. Empty means they
 * all agree — the caller decides whether that is a failure.
 */
function hostDisagreements() {
  const problems = [];
  for (const host of HOSTS) {
    let source;
    try {
      source = fs.readFileSync(host.file, "utf8");
    } catch (err) {
      problems.push(`${host.label}: could not read ${host.file} (${err.message})`);
      continue;
    }
    const found = source.match(host.pattern)?.[1];
    if (!found) {
      problems.push(
        `${host.label}: no bundle public key found. Either it moved, or it is ` +
          `no longer 64 lowercase hex characters — both need looking at.`
      );
    } else if (found !== BUNDLE_PUBLIC_KEY) {
      problems.push(
        `${host.label}: has ${found}\n` +
          `    scripts/bundle-key.js says ${BUNDLE_PUBLIC_KEY}`
      );
    }
  }
  return problems;
}

module.exports = { BUNDLE_PUBLIC_KEY, HOSTS, hostDisagreements };

if (require.main === module) {
  const problems = hostDisagreements();
  if (problems.length) {
    console.error(`\nThe bundle public key has drifted:\n\n  ${problems.join("\n  ")}\n`);
    process.exit(1);
  }
  console.log(`${BUNDLE_PUBLIC_KEY}\nBoth hosts agree.`);
}
