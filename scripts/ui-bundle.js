#!/usr/bin/env node
/**
 * Fetch a published UI bundle from the CDN, verify it, and unpack it.
 *
 *   node scripts/ui-bundle.js android              # newest on the stable channel
 *   node scripts/ui-bundle.js ios 2026-08-25.7     # that one
 *
 * Used by `build-ui.js` as its third source (`plans/repo-split.md` § 3a): a
 * checkout with no access to `hintjen/homerun-app-ui` can still build a real
 * app, because the compiled bundle is a public object and its manifest is
 * signed.
 *
 * # What is trusted, and what is not
 *
 * Nothing from the network is trusted on its face. The manifest carries an
 * Ed25519 signature over the same bytes `sign-manifest.js` signed and
 * `rust/homerun-core/src/bundle.rs` verifies on a device; the archive is
 * checked against the `sha256` in that manifest. So a tampered pointer object
 * fails exactly the way a tampered API reply would, and the CDN gains no trust
 * it did not already have — which is the argument for putting the pointer
 * there at all.
 *
 * The digest proves the archive is the one that was signed. It says nothing
 * about whether the archive is *well behaved*, so the unpack applies the same
 * ceilings and the same path-traversal check `BundleUpdater.unpack` applies on
 * a device. A signed zip bomb is still a zip bomb.
 *
 * # Where the objects live
 *
 * `publish-ui-bundle.yml` writes these, prod only:
 *
 *   ui/latest-<platform>-<channel>.json   the signed manifest, verbatim
 *   <archive key with .zip -> .json>      the same, for pinning by id
 *
 * The pin object sits beside its archive rather than at a path of its own, so
 * the two cannot drift apart — and so `ios` does not collide with `android`,
 * whose serials are independent and can both be at 7 on the same day.
 */
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

const { signingPayload } = require("./sign-manifest");
const { BUNDLE_PUBLIC_KEY } = require("./bundle-key");
const { hostRevision } = require("./host-revision");

const CDN = process.env.HOMERUN_UI_CDN ?? "https://cdn.gethomerun.app";

/**
 * The key manifests are verified against.
 *
 * `HOMERUN_UI_BUNDLE_KEY` overrides it for testing against a throwaway key,
 * exactly as `-PbundlePublicKey=` does in `android/app/build.gradle.kts`, and
 * it is announced when used because a build verifying against a key nobody
 * recognises should never be a quiet one.
 *
 * It grants no reach an attacker did not already have: anyone who can set this
 * process's environment can set `HOMERUN_UI_DIR` to a directory of their own,
 * and that stages whatever they like with no signature involved at all.
 */
const publicKeyInUse = () => {
  const override = process.env.HOMERUN_UI_BUNDLE_KEY;
  if (!override) return BUNDLE_PUBLIC_KEY;
  console.warn(
    `\nWARNING: verifying against HOMERUN_UI_BUNDLE_KEY (${override.slice(0, 12)}…),\n` +
      "         not the key both hosts ship. Do not release this build.\n"
  );
  return override;
};

/**
 * The ceilings `BundleUpdater.kt` applies, for the same reasons. Kept in the
 * same units and the same order so the two read as one decision.
 */
const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024;
const MAX_UNPACKED_BYTES = 256 * 1024 * 1024;
const MAX_ENTRIES = 10_000;

/**
 * The prefix a platform's objects live under.
 *
 * Mirrors the rule in `publish-ui-bundle.yml`'s *Ask the API what serial to
 * sign*: android keeps the bare path because its archives are already
 * published under it and a signed manifest names the URL; every other platform
 * takes a segment of its own. Two copies of this rule, in two languages —
 * change one and the pin URLs stop resolving, which is at least loud.
 */
const prefixFor = (platform) => (platform === "android" ? "ui" : `ui/${platform}`);

/** Where the pointer to the newest bundle on a channel lives. */
const latestUrl = (platform, channel) =>
  `${CDN}/ui/latest-${platform}-${channel}.json`;

/** Where a specific bundle's manifest lives, for pinning. */
const pinUrl = (platform, id) => `${CDN}/${prefixFor(platform)}/${id}.json`;

// --- verification ----------------------------------------------------------

/** Node wants Ed25519 public keys wrapped in DER; this is the fixed prefix. */
function publicKeyFromHex(hex) {
  if (!/^[0-9a-f]{64}$/i.test(hex)) {
    throw new Error(`The bundle public key must be 64 hex characters, got: ${hex}`);
  }
  return crypto.createPublicKey({
    key: Buffer.concat([
      Buffer.from("302a300506032b6570032100", "hex"),
      Buffer.from(hex, "hex"),
    ]),
    format: "der",
    type: "spki",
  });
}

const REQUIRED = ["bundle", "url", "sha256", "minHost", "serial", "platform", "signature"];

/**
 * Check a manifest is what it claims to be, or throw saying why.
 *
 * Shape first, then signature. A missing field would otherwise reach
 * `signingPayload` as `undefined`, join into the payload as the string
 * "undefined", and fail the signature check — which is the right answer given
 * for entirely the wrong reason, and the message would send someone hunting a
 * key mismatch that is not there.
 */
function verifyManifest(manifest, { platform, publicKey = publicKeyInUse() }) {
  const missing = REQUIRED.filter((f) => manifest[f] === undefined || manifest[f] === null);
  if (missing.length) {
    throw new Error(`The manifest is missing ${missing.join(", ")}.`);
  }
  if (manifest.platform !== platform) {
    throw new Error(
      `This manifest is for ${manifest.platform}, not ${platform}. ` +
        "The pointer object and the platform asked for do not agree."
    );
  }
  if (!/^[0-9a-f]{128}$/i.test(manifest.signature)) {
    throw new Error("The manifest's signature is not 128 hex characters.");
  }
  if (!manifest.url.startsWith(`${CDN}/`)) {
    // The signature covers `url`, so a signed manifest cannot be redirected —
    // but a manifest signed for a different deployment should not send this
    // build somewhere unexpected either, and the check is free.
    throw new Error(`The manifest names ${manifest.url}, which is not on ${CDN}.`);
  }

  const ok = crypto.verify(
    null,
    Buffer.from(signingPayload(manifest)),
    publicKeyFromHex(publicKey),
    Buffer.from(manifest.signature, "hex")
  );
  if (!ok) {
    throw new Error(
      "The manifest's signature does not verify against the bundle public key.\n" +
        "Either it was tampered with, or this checkout carries a different key\n" +
        "than the one CI signs with (scripts/bundle-key.js)."
    );
  }
  return manifest;
}

/**
 * Refuse a bundle this checkout's host could not serve.
 *
 * The device's own rule (`bundle.rs`: `min_host > host_revision` is a refusal)
 * applied at build time. Embedding a UI that calls a channel the host does not
 * answer does not fail loudly — the invoke never resolves and the screen sits
 * there, which is the failure `CLAUDE.md` singles out.
 */
function checkMinHost(manifest, platform) {
  const revision = hostRevision(platform);
  if (manifest.minHost > revision) {
    throw new Error(
      `Bundle ${manifest.bundle} needs host revision ${manifest.minHost}, ` +
        `and this checkout's ${platform} host is at ${revision}.\n` +
        "Update the checkout, or pin an older bundle with HOMERUN_UI_BUNDLE."
    );
  }
  return revision;
}

// --- fetching --------------------------------------------------------------

async function getJson(url, what) {
  const res = await fetch(url);
  if (res.status === 403 || res.status === 404) {
    throw new Error(
      `No ${what} at ${url} (HTTP ${res.status}).\n` +
        "Nothing has been published there, or the id is wrong."
    );
  }
  if (!res.ok) throw new Error(`Could not fetch ${what}: HTTP ${res.status} from ${url}`);
  const text = await res.text();
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${url} did not return JSON. First bytes: ${text.slice(0, 80)}`);
  }
}

async function getArchive(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`Could not fetch the archive: HTTP ${res.status} from ${url}`);

  // Checked before reading, when the header is there to check. A device does
  // the same, and for the same reason: the digest would catch a substituted
  // archive, but only after it had been written to disk.
  const declared = Number(res.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > MAX_ARCHIVE_BYTES) {
    throw new Error(`The archive claims ${declared} bytes, over the ${MAX_ARCHIVE_BYTES} ceiling.`);
  }
  const body = Buffer.from(await res.arrayBuffer());
  if (body.length > MAX_ARCHIVE_BYTES) {
    throw new Error(`The archive is ${body.length} bytes, over the ${MAX_ARCHIVE_BYTES} ceiling.`);
  }
  return body;
}

// --- unpacking -------------------------------------------------------------

/**
 * The entries of a zip, from its central directory.
 *
 * Written out rather than taking a dependency: this script has to run on a
 * checkout whose `npm ci` may not have happened yet, which is the whole point
 * of it. It reads the central directory rather than streaming local headers,
 * because only the central directory is authoritative about what an archive
 * contains — a local header can disagree with it, and the disagreement is a
 * known way to smuggle an entry past a naive reader.
 */
function zipEntries(buf) {
  const EOCD = 0x06054b50;
  const CENTRAL = 0x02014b50;

  let eocd = -1;
  const earliest = Math.max(0, buf.length - 0xffff - 22);
  for (let i = buf.length - 22; i >= earliest; i -= 1) {
    if (buf.readUInt32LE(i) === EOCD) { eocd = i; break; }
  }
  if (eocd < 0) throw new Error("Not a zip archive: no end-of-central-directory record.");

  const count = buf.readUInt16LE(eocd + 10);
  const size = buf.readUInt32LE(eocd + 12);
  const offset = buf.readUInt32LE(eocd + 16);
  if (count === 0xffff || size === 0xffffffff || offset === 0xffffffff) {
    // A UI bundle is ~3.5 MB. Anything needing zip64 is not one, and guessing
    // at the format is how a reader ends up trusting the wrong offsets.
    throw new Error("The archive uses zip64, which a UI bundle never needs.");
  }
  if (count > MAX_ENTRIES) throw new Error(`The archive has ${count} entries, over ${MAX_ENTRIES}.`);

  const entries = [];
  let p = offset;
  for (let i = 0; i < count; i += 1) {
    if (buf.readUInt32LE(p) !== CENTRAL) throw new Error("Malformed central directory.");
    const method = buf.readUInt16LE(p + 10);
    const compressed = buf.readUInt32LE(p + 20);
    const uncompressed = buf.readUInt32LE(p + 24);
    const nameLen = buf.readUInt16LE(p + 28);
    const extraLen = buf.readUInt16LE(p + 30);
    const commentLen = buf.readUInt16LE(p + 32);
    const local = buf.readUInt32LE(p + 42);
    const name = buf.subarray(p + 46, p + 46 + nameLen).toString("utf8");
    entries.push({ name, method, compressed, uncompressed, local });
    p += 46 + nameLen + extraLen + commentLen;
  }
  return entries;
}

/** The bytes of one entry, inflated, capped at `remaining`. */
function entryBytes(buf, entry, remaining) {
  const LOCAL = 0x04034b50;
  if (buf.readUInt32LE(entry.local) !== LOCAL) {
    throw new Error(`Malformed local header for ${entry.name}.`);
  }
  const nameLen = buf.readUInt16LE(entry.local + 26);
  const extraLen = buf.readUInt16LE(entry.local + 28);
  const start = entry.local + 30 + nameLen + extraLen;
  const raw = buf.subarray(start, start + entry.compressed);

  if (entry.method === 0) {
    if (raw.length > remaining) throw new Error(bombMessage());
    return raw;
  }
  if (entry.method !== 8) {
    throw new Error(`${entry.name} uses compression method ${entry.method}; only stored and deflate are read.`);
  }
  // maxOutputLength is the real ceiling. Trusting the entry's declared
  // uncompressed size would be trusting the archive to describe its own bomb.
  let out;
  try {
    out = zlib.inflateRawSync(raw, { maxOutputLength: Math.max(1, remaining) });
  } catch (err) {
    if (/maxOutputLength|buffer/i.test(err.message)) throw new Error(bombMessage());
    throw new Error(`Could not inflate ${entry.name}: ${err.message}`);
  }
  if (out.length !== entry.uncompressed) {
    throw new Error(
      `${entry.name} inflated to ${out.length} bytes but the directory says ` +
        `${entry.uncompressed}. The archive disagrees with itself.`
    );
  }
  return out;
}

const bombMessage = () =>
  `The archive expands to more than ${MAX_UNPACKED_BYTES} bytes.`;

/**
 * Unpack `buf` into `into`, refusing anything that tries to leave it.
 *
 * `../../databases/homerun.db` is a valid zip entry name and `path.join`
 * resolves it happily — that is Zip Slip, and here it would be a file write
 * anywhere the person running the build can reach. The resolved-prefix check
 * is the same one `BundleUpdater.unpack` performs.
 */
function unpack(buf, into) {
  fs.mkdirSync(into, { recursive: true });
  // Resolved, not just absolute: on macOS the temp directory is a symlink, and
  // comparing an unresolved prefix against a resolved target rejects every
  // entry — which would read as a hostile archive rather than a broken check.
  const root = fs.realpathSync(into);
  let written = 0;
  let files = 0;

  for (const entry of zipEntries(buf)) {
    if (entry.name.endsWith("/")) continue; // a directory record; the files make them

    // Rejected by name before anything resolves it, so the reason can be
    // specific. An absolute path and a drive letter are the two shapes the
    // prefix check below would catch anyway but describe badly.
    if (path.isAbsolute(entry.name) || /^[a-zA-Z]:/.test(entry.name) || entry.name.includes("\\")) {
      throw new Error(`The archive contains an absolute or windows-style path: ${entry.name}`);
    }
    const target = path.resolve(root, entry.name);
    if (target !== root && !target.startsWith(root + path.sep)) {
      throw new Error(`The archive contains an entry outside itself: ${entry.name}`);
    }

    const bytes = entryBytes(buf, entry, MAX_UNPACKED_BYTES - written);
    written += bytes.length;
    if (written > MAX_UNPACKED_BYTES) throw new Error(bombMessage());
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, bytes);
    files += 1;
  }

  if (!fs.existsSync(path.join(root, "index.html"))) {
    // The same completeness marker `build-ui.js` and `BundleStore.stage` use.
    // An archive without one is not a UI, and staging it would trade a working
    // app for a blank screen.
    throw new Error("The archive has no index.html at its root, so it is not a UI bundle.");
  }
  return { files, bytes: written };
}

// --- the whole job ---------------------------------------------------------

/**
 * Resolve, verify and unpack a bundle into `into`. Returns its manifest.
 *
 * `id` pins; without one this takes whatever `channel` currently points at.
 */
async function fetchBundle({ platform, id, channel = "stable", into, publicKey }) {
  const url = id ? pinUrl(platform, id) : latestUrl(platform, channel);
  const what = id ? `manifest for ${id}` : `${channel} pointer for ${platform}`;
  console.log(`Shared UI: ${url}`);

  const manifest = await getJson(url, what);
  verifyManifest(manifest, { platform, publicKey });
  const revision = checkMinHost(manifest, platform);

  const archive = await getArchive(manifest.url);
  const digest = crypto.createHash("sha256").update(archive).digest("hex");
  if (digest !== manifest.sha256) {
    throw new Error(
      `The archive does not match its manifest.\n` +
        `  expected ${manifest.sha256}\n  got      ${digest}`
    );
  }

  fs.rmSync(into, { recursive: true, force: true });
  const { files, bytes } = unpack(archive, into);
  console.log(
    `Shared UI: ${manifest.bundle} verified — serial ${manifest.serial}, ` +
      `minHost ${manifest.minHost} (host is ${revision}), ${files} files, ` +
      `${Math.round(bytes / 1024)} KiB`
  );
  return manifest;
}

module.exports = {
  CDN,
  MAX_ARCHIVE_BYTES,
  MAX_UNPACKED_BYTES,
  MAX_ENTRIES,
  prefixFor,
  latestUrl,
  pinUrl,
  publicKeyFromHex,
  verifyManifest,
  checkMinHost,
  zipEntries,
  unpack,
  fetchBundle,
};

if (require.main === module) {
  const [platform, id] = process.argv.slice(2);
  if (!platform) {
    console.error("\nUsage: ui-bundle.js <android|ios> [bundle-id]\n");
    process.exit(2);
  }
  const into = path.join(require("os").tmpdir(), `homerun-ui-${platform}-${process.pid}`);
  fetchBundle({ platform, id, into })
    .then((m) => console.log(`\nUnpacked ${m.bundle} into ${into}`))
    .catch((err) => {
      console.error(`\n${err.message}\n`);
      process.exit(1);
    });
}
