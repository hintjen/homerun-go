#!/usr/bin/env node
/**
 * Sign an over-the-air bundle manifest, and generate the key that signs it.
 *
 *   node scripts/sign-manifest.js keygen
 *   node scripts/sign-manifest.js sign --archive ui.zip --bundle 2026-08-14.1 \
 *        --url https://cdn.gethomerun.app/ui/2026-08-14.1.zip --serial 1
 *
 * # Why this lives here
 *
 * The bytes a signature covers are defined by `Manifest::signing_payload` in
 * `rust/homerun-core/src/bundle.rs`, and a format with one implementation is
 * not a format — it is a habit. This is the second implementation, deliberately
 * in the repo that holds the first, so the two are read together and a change
 * to one is an obviously incomplete change.
 *
 * The publish workflow in the UI repo is the third caller, not a third
 * implementation: it runs this.
 *
 * # The private key
 *
 * `keygen` prints both halves and writes neither. The public half goes into the
 * app at build time (`-PbundlePublicKey=…`); the private half goes into the CI
 * secret store and nowhere else — not a file in a repo, not a password manager
 * note, not this terminal's scrollback if you can help it.
 *
 *   HOMERUN_BUNDLE_KEY   the private key to sign with, 64 hex characters.
 *                        An argument would put it in the process list.
 */
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

/** Must match `PAYLOAD_VERSION` in homerun-core. */
const PAYLOAD_VERSION = "homerun-bundle-v1";

const die = (message) => {
  console.error(`\n${message}\n`);
  process.exit(1);
};

// --- keys ------------------------------------------------------------------

/**
 * Ed25519 keys are 32 bytes, but Node only hands them over wrapped in DER.
 * The last 32 bytes of an SPKI public key and of a PKCS#8 private key are the
 * raw halves — true for Ed25519 specifically, because both structures are a
 * fixed-length prefix followed by the key.
 */
const rawPublic = (key) => key.export({ type: "spki", format: "der" }).subarray(-32);
const rawPrivate = (key) => key.export({ type: "pkcs8", format: "der" }).subarray(-32);

/** Rebuild a signing key from the 32 raw bytes, via the DER prefix Node wants. */
function privateKeyFromHex(hex) {
  if (!/^[0-9a-f]{64}$/i.test(hex)) die("HOMERUN_BUNDLE_KEY must be 64 hex characters.");
  const prefix = Buffer.from("302e020100300506032b657004220420", "hex");
  return crypto.createPrivateKey({
    key: Buffer.concat([prefix, Buffer.from(hex, "hex")]),
    format: "der",
    type: "pkcs8",
  });
}

function keygen() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync("ed25519");
  console.log(`
Public  (into the app):   ${rawPublic(publicKey).toString("hex")}
Private (into CI only):   ${rawPrivate(privateKey).toString("hex")}

The private half is shown once and stored nowhere. Put it in the CI secret
store now; if it is lost, generate a new pair and ship an app update carrying
the new public half — every device holds the old one until it does.
`);
}

// --- signing ---------------------------------------------------------------

/**
 * The signed bytes. Field per line, version first, trailing newline.
 *
 * Not canonical JSON: that would make the signer and the verifier agree on key
 * order, unicode escaping and number rendering, and when they disagree the
 * symptom is a valid signature that will not verify — which looks exactly like
 * an attack and gets "fixed" by weakening the check.
 */
function signingPayload(m) {
  for (const [name, value] of Object.entries(m)) {
    if (typeof value === "string" && /[\r\n]/.test(value)) {
      die(`The manifest's ${name} contains a line break, which would make the signed bytes ambiguous.`);
    }
  }
  return [PAYLOAD_VERSION, m.bundle, m.url, m.sha256, m.minHost, m.serial, m.platform].join("\n") + "\n";
}

function sign(args) {
  const need = (name) => args[name] ?? die(`Missing --${name}.`);

  const archive = need("archive");
  if (!fs.existsSync(archive)) die(`No such archive: ${archive}`);
  const sha256 = crypto.createHash("sha256").update(fs.readFileSync(archive)).digest("hex");

  const manifest = {
    bundle: need("bundle"),
    url: need("url"),
    sha256,
    // Both spellings: `--minHost` matches the manifest field, `--min-host` is
    // what anyone writing a shell script will reach for first. Accepting only
    // one would silently fall back to 1, and a minHost that is too low is a UI
    // calling channels an old host cannot answer — which hangs rather than errors.
    minHost: Number(args.minHost ?? args["min-host"] ?? 1),
    // Monotonic, and the client refuses anything not strictly greater than
    // what it is running. Rolling back is therefore a *new* serial carrying
    // older content — which is what stops a replayed old manifest from
    // downgrading every device to a version whose bugs are known.
    serial: Number(need("serial")),
    platform: args.platform ?? "android",
  };
  if (!Number.isInteger(manifest.serial) || manifest.serial < 1) {
    die("--serial must be a whole number, 1 or more.");
  }
  if (!manifest.url.startsWith("https://")) die("--url must be https.");

  const key = privateKeyFromHex(process.env.HOMERUN_BUNDLE_KEY ?? die(
    "Set HOMERUN_BUNDLE_KEY to the private key, 64 hex characters.\n" +
      "Passing it as an argument would put it in the process list."
  ));
  // `null` is the digest algorithm: Ed25519 hashes internally and Node rejects
  // being told which algorithm to use.
  manifest.signature = crypto.sign(null, Buffer.from(signingPayload(manifest)), key).toString("hex");

  const json = JSON.stringify(manifest, null, 2);
  if (args.out) {
    fs.mkdirSync(path.dirname(path.resolve(args.out)), { recursive: true });
    fs.writeFileSync(args.out, json);
    console.error(`Wrote ${args.out} — bundle ${manifest.bundle}, serial ${manifest.serial}`);
  } else {
    console.log(json);
  }
}

// --- entry -----------------------------------------------------------------

const [command, ...rest] = process.argv.slice(2);
const args = {};
for (let i = 0; i < rest.length; i += 1) {
  if (!rest[i].startsWith("--")) continue;
  args[rest[i].slice(2)] = rest[i + 1]?.startsWith("--") ? true : rest[++i];
}

if (command === "keygen") keygen();
else if (command === "sign") sign(args);
else die("Usage: sign-manifest.js keygen | sign --archive <zip> --bundle <id> --url <https> --serial <n>");
