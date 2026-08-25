#!/usr/bin/env node
/**
 * Proves `ui-bundle.js` refuses what it is supposed to refuse.
 *
 *   node scripts/check-ui-bundle.js
 *
 * # Why this exists and the others do not
 *
 * The rest of `scripts/` is checkers: they read the tree and fail on a drift,
 * and their own failure mode is "reads nothing and fails", which is safe.
 * `ui-bundle.js` is different in kind — it is the only JavaScript here that
 * decides whether to *trust* something, and every one of its checks fails
 * open if it is wrong. A signature check that always returns true, a digest
 * comparison against the wrong variable, a Zip Slip guard that resolves the
 * prefix on the wrong side: all four look exactly like a working build.
 *
 * So each guard is exercised against an input that should trip it, with a
 * throwaway signing key generated per run — the real one signs nothing here.
 * A happy path alone would pass with every check deleted, which is the point
 * `tests-that-bite` makes.
 *
 * No server and no network: the fixtures go through the same functions
 * `fetchBundle` calls, and the one thing not covered is `fetch` itself.
 */
const assert = require("assert");
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const zlib = require("zlib");

const { signingPayload } = require("./sign-manifest");
const { hostRevision } = require("./host-revision");
const bundle = require("./ui-bundle");

// --- fixtures --------------------------------------------------------------

const { publicKey, privateKey } = crypto.generateKeyPairSync("ed25519");
const PUBLIC_HEX = publicKey.export({ type: "spki", format: "der" }).subarray(-32).toString("hex");

/** A manifest signed with the throwaway key, with `overrides` applied first. */
function signed(overrides = {}) {
  const manifest = {
    bundle: "2026-08-25.1",
    url: `${bundle.CDN}/ui/2026-08-25.1.zip`,
    sha256: "0".repeat(64),
    minHost: 1,
    serial: 1,
    platform: "android",
    ...overrides,
  };
  manifest.signature = crypto
    .sign(null, Buffer.from(signingPayload(manifest)), privateKey)
    .toString("hex");
  return manifest;
}

/**
 * A zip built the way `publish-ui-bundle.yml` builds one: deflate, no
 * directory records, `index.html` at the root.
 */
function zip(entries) {
  const locals = [];
  const central = [];
  let offset = 0;

  for (const [name, body] of entries) {
    const raw = Buffer.from(body);
    const deflated = zlib.deflateRawSync(raw);
    const nameBuf = Buffer.from(name, "utf8");
    const crc = zlib.crc32 ? zlib.crc32(raw) : 0;

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(8, 8); // deflate
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(deflated.length, 18);
    local.writeUInt32LE(raw.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    locals.push(local, nameBuf, deflated);

    const dir = Buffer.alloc(46);
    dir.writeUInt32LE(0x02014b50, 0);
    dir.writeUInt16LE(20, 6);
    dir.writeUInt16LE(8, 10);
    dir.writeUInt32LE(crc, 16);
    dir.writeUInt32LE(deflated.length, 20);
    dir.writeUInt32LE(raw.length, 24);
    dir.writeUInt16LE(nameBuf.length, 28);
    dir.writeUInt32LE(offset, 42);
    central.push(dir, nameBuf);

    offset += 30 + nameBuf.length + deflated.length;
  }

  const body = Buffer.concat(locals);
  const dirBytes = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(dirBytes.length, 12);
  eocd.writeUInt32LE(body.length, 16);
  return Buffer.concat([body, dirBytes, eocd]);
}

const GOOD_ZIP = [["index.html", "<!doctype html>hi"], ["_next/app.js", "console.log(1)"]];

// --- the harness -----------------------------------------------------------

let failures = 0;
const work = fs.mkdtempSync(path.join(os.tmpdir(), "homerun-uibundle-test-"));
let n = 0;
const into = () => path.join(work, `case-${++n}`);

function ok(label, fn) {
  try {
    fn();
    console.log(`  PASS  ${label}`);
  } catch (err) {
    failures += 1;
    console.error(`  FAIL  ${label}\n        ${err.message.split("\n")[0]}`);
  }
}

/** `fn` must throw, and the message must mention `expect`. */
function refuses(label, expect, fn) {
  try {
    fn();
  } catch (err) {
    if (err.message.toLowerCase().includes(expect.toLowerCase())) {
      console.log(`  PASS  ${label}`);
      return;
    }
    failures += 1;
    console.error(`  FAIL  ${label}\n        threw, but not about "${expect}": ${err.message.split("\n")[0]}`);
    return;
  }
  failures += 1;
  console.error(`  FAIL  ${label}\n        did not throw at all — the guard is inert`);
}

const verify = (m) => bundle.verifyManifest(m, { platform: "android", publicKey: PUBLIC_HEX });

console.log("\nManifest verification");
ok("a correctly signed manifest verifies", () => verify(signed()));

refuses("a flipped serial breaks the signature", "signature does not verify", () => {
  const m = signed();
  m.serial = 99; // signed field, changed after signing
  verify(m);
});
refuses("a flipped url breaks the signature", "signature does not verify", () => {
  const m = signed();
  m.url = `${bundle.CDN}/ui/evil.zip`;
  verify(m);
});
refuses("a flipped minHost breaks the signature", "signature does not verify", () => {
  const m = signed();
  m.minHost = 1000;
  verify(m);
});
refuses("a manifest signed by another key is refused", "signature does not verify", () => {
  const other = crypto.generateKeyPairSync("ed25519");
  const hex = other.publicKey.export({ type: "spki", format: "der" }).subarray(-32).toString("hex");
  bundle.verifyManifest(signed(), { platform: "android", publicKey: hex });
});
refuses("a missing field is named, not fed to the payload", "missing sha256", () => {
  const m = signed();
  delete m.sha256;
  verify(m);
});
refuses("another platform's manifest is refused", "not ios", () =>
  bundle.verifyManifest(signed(), { platform: "ios", publicKey: PUBLIC_HEX })
);
refuses("a url off the CDN is refused", "not on", () =>
  verify(signed({ url: "https://example.invalid/ui/x.zip" }))
);
refuses("a short signature is refused before verifying", "128 hex", () => {
  const m = signed();
  m.signature = "abcd";
  verify(m);
});

console.log("\nminHost");
const revision = hostRevision("android");
ok(`minHost at this checkout's revision (${revision}) is accepted`, () =>
  bundle.checkMinHost({ bundle: "b", minHost: revision }, "android")
);
refuses("minHost above it is refused", "needs host revision", () =>
  bundle.checkMinHost({ bundle: "b", minHost: revision + 1 }, "android")
);

console.log("\nUnpacking");
ok("a well-formed archive unpacks", () => {
  const dir = into();
  const { files } = bundle.unpack(zip(GOOD_ZIP), dir);
  assert.strictEqual(files, 2);
  assert.strictEqual(fs.readFileSync(path.join(dir, "index.html"), "utf8"), "<!doctype html>hi");
  assert.ok(fs.existsSync(path.join(dir, "_next", "app.js")), "nested entry was not written");
});
refuses("an archive with no index.html is refused", "no index.html", () =>
  bundle.unpack(zip([["about.html", "x"]]), into())
);
refuses("a Zip Slip entry is refused", "outside itself", () =>
  bundle.unpack(zip([["index.html", "x"], ["../../escaped.txt", "pwned"]]), into())
);
refuses("an absolute path is refused", "absolute", () =>
  bundle.unpack(zip([["index.html", "x"], ["/etc/passwd", "pwned"]]), into())
);
refuses("a windows-style path is refused", "absolute or windows", () =>
  bundle.unpack(zip([["index.html", "x"], ["..\\escaped.txt", "pwned"]]), into())
);
refuses("more entries than the ceiling is refused", "over", () => {
  const many = Array.from({ length: bundle.MAX_ENTRIES + 1 }, (_, i) => [`f${i}.txt`, "x"]);
  bundle.unpack(zip(many), into());
});
ok("Zip Slip is caught before the file is written", () => {
  const dir = into();
  const escaped = path.resolve(dir, "..", "..", "escaped.txt");
  fs.rmSync(escaped, { force: true });
  try {
    bundle.unpack(zip([["index.html", "x"], ["../../escaped.txt", "pwned"]]), dir);
  } catch {
    /* expected — what matters is what is on disk */
  }
  assert.ok(!fs.existsSync(escaped), `it wrote ${escaped} before refusing`);
});
refuses("an entry that lies about its inflated size is refused", "disagrees with itself", () => {
  const buf = zip([["index.html", "x".repeat(500)]]);
  // The central directory's uncompressed-size field for the first entry. The
  // local header keeps the truth, so this is the two disagreeing.
  const eocd = buf.length - 22;
  const cdOffset = buf.readUInt32LE(eocd + 16);
  buf.writeUInt32LE(499, cdOffset + 24);
  bundle.unpack(buf, into());
});

console.log("\nURL layout");
ok("android keeps the bare prefix, other platforms take a segment", () => {
  assert.strictEqual(bundle.prefixFor("android"), "ui");
  assert.strictEqual(bundle.prefixFor("ios"), "ui/ios");
  assert.strictEqual(bundle.pinUrl("android", "2026-08-25.7"), `${bundle.CDN}/ui/2026-08-25.7.zip`.replace(".zip", ".json"));
  assert.strictEqual(bundle.pinUrl("ios", "2026-08-25.7"), `${bundle.CDN}/ui/ios/2026-08-25.7.json`);
  assert.strictEqual(
    bundle.latestUrl("android", "stable"),
    `${bundle.CDN}/ui/latest-android-stable.json`
  );
  // The two platforms must not collide on the pin object, which is the whole
  // reason it sits beside the archive rather than at `ui/<id>.json`.
  assert.notStrictEqual(bundle.pinUrl("android", "x"), bundle.pinUrl("ios", "x"));
});

fs.rmSync(work, { recursive: true, force: true });

if (failures) {
  console.error(`\n${failures} check(s) failed — ui-bundle.js is trusting something it should not.\n`);
  process.exit(1);
}
console.log("\nPASS — every guard in ui-bundle.js refuses what it is meant to.");
