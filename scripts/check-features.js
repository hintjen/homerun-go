#!/usr/bin/env node
/*
  Does a built library actually have the cargo features it was built for?

  Every feature here is a capability the FFI refuses in a sentence when it is
  compiled out. That sentence is the evidence: it is in the binary exactly when
  the feature is not, so one `grep` answers the question from the artifact
  itself rather than from the build log that claimed to produce it.

  This exists because the claim and the artifact came apart once. A shared
  CARGO_TARGET_DIR let `homerun-pumpkin-bin` build the same cdylib with
  features = ["pumpkin-engine"] into the path the `android` target stages
  from, and a build that reported success in 3.05s staged the wrong library.
  It reached Google Play as 0.1.0 (1013), where every server launch -- Java
  and Pumpkin alike, since Android spawns both as child processes -- failed
  with the first message below. Nothing in the build log said so; the only
  honest witness was the artifact.

  Run it against anything, including a library pulled off a phone:

    node scripts/check-features.js libhomerun_pumpkin_ffi.so process-engine device-ws
*/
const fs = require("fs");

/**
 * feature -> what the build says when that feature is missing.
 *
 * Only `homerun-pumpkin-ffi` has these. Keep them byte-identical to the
 * `#[cfg(not(feature = ...))]` arms in `rust/homerun-pumpkin-ffi/src/lib.rs`:
 * a marker that no longer matches its source reads as "feature present" in the
 * absent direction, which is why both directions are checked below.
 */
const FEATURE_MARKERS = {
  "process-engine": "This build cannot run a server as a separate process.",
  "device-ws": "This build cannot serve a device websocket.",
};

/**
 * Every way `bytes` disagrees with `features`, as sentences. Empty means it is
 * the library that was asked for.
 *
 * Checked in both directions deliberately. "Marker absent" alone would also be
 * true of a build where the string was renamed or the fallback deleted, and
 * the entire point is to not trust a file we merely found on disk.
 */
function featureProblems(bytes, features) {
  const problems = [];
  for (const [feature, marker] of Object.entries(FEATURE_MARKERS)) {
    const wanted = features.includes(feature);
    const missing = bytes.includes(marker);
    if (wanted && missing) {
      problems.push(`${feature} was requested, but the binary still says "${marker}"`);
    }
    if (!wanted && missing === false) {
      problems.push(
        `${feature} was not requested, so "${marker}" should be in the binary and is not`
      );
    }
  }
  return problems;
}

module.exports = { FEATURE_MARKERS, featureProblems };

if (require.main === module) {
  const [file, ...features] = process.argv.slice(2);
  if (!file) {
    console.error(
      "usage: check-features.js <binary> [feature...]\n" +
        `  features: ${Object.keys(FEATURE_MARKERS).join(", ")}`
    );
    process.exit(2);
  }
  const unknown = features.filter((f) => !(f in FEATURE_MARKERS));
  if (unknown.length) {
    // Silently ignoring one would make this pass for the wrong reason.
    console.error(`No marker is known for: ${unknown.join(", ")}`);
    process.exit(2);
  }
  const problems = featureProblems(fs.readFileSync(file), features);
  const asked = features.length ? features.join(", ") : "none";
  if (problems.length) {
    console.error(`${file} is not the build that was asked for.`);
    for (const p of problems) console.error(`  - ${p}`);
    console.error(`\n  Requested: ${asked}`);
    process.exit(1);
  }
  console.log(`${file}: features verified (${asked})`);
}
