#!/usr/bin/env node
/**
 * Assert that each host's expected FFI ABI matches the crate's.
 *
 * `NativeServer` refuses to load a library whose ABI it does not recognise,
 * which is the right instinct — a `.so` that links but decodes garbage is the
 * failure it exists to catch. The trouble is that the expectation is a
 * hand-maintained constant, and the check that enforces it only runs when
 * something touches the object holding it.
 *
 * Both halves of that failed at once. The crate went to 2 with the backup
 * work and the Kotlin constant stayed at 1, and nobody found out, because the
 * only backend that touches `NativeServer` was not the one being used. The
 * moment something did, the server would have been disabled with a single
 * logcat line to explain it.
 *
 * So the comparison happens here, at build time, where forgetting is loud and
 * free. Same idea as `check-coverage.js`: read the source rather than trust a
 * comment.
 */
const fs = require("fs");
const path = require("path");

const { ROOT } = require("./targets");

/** Where each expectation lives, and how to recognise it. */
const EXPECTATIONS = [
  {
    label: "Android (NativeServer.kt)",
    file: path.join(
      ROOT, "android", "app", "src", "main", "java", "app", "gethomerun",
      "mobile", "NativeServer.kt"
    ),
    pattern: /EXPECTED_ABI\s*=\s*(\d+)/,
  },
];

const CRATE = path.join(ROOT, "rust", "homerun-pumpkin-ffi", "src", "lib.rs");

function read(file, pattern, what) {
  let source;
  try {
    source = fs.readFileSync(file, "utf8");
  } catch (err) {
    console.error(`\nCould not read ${what}:\n  ${file}\n  ${err.message}\n`);
    process.exit(1);
  }
  const match = source.match(pattern);
  if (!match) {
    console.error(
      `\nCould not find ${what} in\n  ${file}\n\n` +
        "The pattern in scripts/check-abi.js no longer matches. Fix the\n" +
        "pattern rather than deleting the check — an ABI that drifts unnoticed\n" +
        "is exactly what this exists to prevent.\n"
    );
    process.exit(1);
  }
  return Number(match[1]);
}

const actual = read(CRATE, /FFI_ABI_VERSION:\s*u32\s*=\s*(\d+)/, "FFI_ABI_VERSION");

let failed = false;
for (const { label, file, pattern } of EXPECTATIONS) {
  const expected = read(file, pattern, `the expected ABI for ${label}`);
  if (expected === actual) {
    console.log(`  ok    ${label} expects ABI ${expected}`);
    continue;
  }
  failed = true;
  console.error(
    `  FAIL  ${label} expects ABI ${expected}, but the crate is at ${actual}\n` +
      `        ${path.relative(ROOT, file)}\n` +
      "        Bump it, or the host will refuse to load the library it just built."
  );
}

if (failed) {
  console.error("\nFFI ABI mismatch.\n");
  process.exit(1);
}
console.log(`\nPASS — every host expects ABI ${actual}.`);
