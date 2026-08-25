#!/usr/bin/env node
/**
 * Where each host writes its bridge revision, and how to read it.
 *
 *   node scripts/host-revision.js          # print both
 *
 * # Why this is its own file
 *
 * Two things need this fact and they are not the same job.
 * `check-host-revision.js` compares each host's revision against the ledger;
 * `build-ui.js` refuses an over-the-air bundle whose `minHost` is above the
 * revision of the checkout it is staging into (`plans/repo-split.md` § 3a).
 *
 * The second is the device's own judgement (`Manifest::min_host` in
 * `rust/homerun-core/src/bundle.rs`) applied at build time: a build must never
 * embed a UI that calls a channel its host cannot answer, because the symptom
 * is an invoke that never resolves — a frozen screen with no error, which
 * `CLAUDE.md` names as the worst failure in this protocol.
 *
 * A regex rather than a parser, for the reason `check-coverage.js` gives: this
 * runs on a CI box with node and nothing else. It fails closed — no match is
 * an error, never a default.
 */
const fs = require("fs");
const path = require("path");

const { ROOT } = require("./targets");

/** Each host's router, how it names its revision, and what it is called. */
const HOSTS = [
  {
    profile: "android",
    label: "Android (BridgeRouter.kt)",
    file: path.join(
      ROOT, "android", "app", "src", "main", "java", "app", "gethomerun",
      "mobile", "BridgeRouter.kt"
    ),
    pattern: /HOST_REVISION\s*=\s*(\d+)/,
  },
  {
    profile: "ios",
    label: "iOS (BridgeRouter.swift)",
    file: path.join(ROOT, "ios", "HomerunHost", "BridgeRouter.swift"),
    pattern: /hostRevision\s*=\s*(\d+)/,
  },
];

/**
 * The revision `profile`'s router claims. Throws rather than guessing: a
 * default here would let a bundle through on the strength of a number nobody
 * wrote down.
 */
function hostRevision(profile) {
  const host = HOSTS.find((h) => h.profile === profile);
  if (!host) throw new Error(`No host router known for "${profile}".`);

  let text;
  try {
    text = fs.readFileSync(host.file, "utf8");
  } catch (err) {
    throw new Error(`Could not read the router for ${host.label}:\n  ${host.file}\n  ${err.message}`);
  }

  const found = text.match(host.pattern)?.[1];
  if (!found) {
    throw new Error(
      `No revision found in ${host.label}. It should read like ` +
        `${host.pattern.source.replace(/\\s\*/g, " ").replace("(\\d+)", "N")}.`
    );
  }
  return Number(found);
}

module.exports = { HOSTS, hostRevision };

if (require.main === module) {
  for (const { profile, label } of HOSTS) {
    console.log(`${profile.padEnd(8)} ${hostRevision(profile)}   ${label}`);
  }
}
