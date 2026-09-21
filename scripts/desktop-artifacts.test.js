const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");
const { prepareArtifacts, LAYOUT, PUBLIC_BASE } = require("./publish-desktop-artifacts");
const { TARGETS } = require("./targets");

const metadata = {
  pumpkin: { minecraftVersion: "1.21.11", protocol: 774 },
  "game-runner": { version: "0.1.0", protocol: 1 },
  "core-node": { version: "0.1.0", abi: 1, sourceBuild: "fixture-revision" },
};
function fixture(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "homerun-artifacts-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  for (const layout of Object.values(LAYOUT)) fs.writeFileSync(path.join(dir, layout.file), `unsigned ${layout.file}`);
  return dir;
}

test("signing changes the download identity and manifest digest", (t) => {
  const dir = fixture(t);
  const before = prepareArtifacts(dir, ["game-runner"], metadata)[0];
  fs.appendFileSync(before.file, " simulated Authenticode signature");
  const after = prepareArtifacts(dir, ["game-runner"], metadata)[0];
  const bytes = fs.readFileSync(after.file);
  const digest = crypto.createHash("sha256").update(bytes).digest("hex");
  assert.notEqual(after.manifest.url, before.manifest.url, "signed bytes must get a new immutable download URL");
  assert.equal(after.manifest.build, digest.slice(0, 12));
  assert.equal(after.manifest.sha256, digest, "the client must verify the bytes it actually downloads");
  assert.equal(after.manifest.size, bytes.length);
  assert.deepEqual(JSON.parse(fs.readFileSync(after.manifestPath)), after.manifest);
});

test("missing or empty inputs cannot partially replace existing manifests", (t) => {
  const dir = fixture(t);
  const manifest = path.join(dir, "game-runner-latest.json");
  fs.writeFileSync(manifest, "previous release");
  fs.writeFileSync(path.join(dir, LAYOUT["core-node"].file), "");
  assert.throws(() => prepareArtifacts(dir, ["game-runner", "core-node"], metadata), /Empty artifact/);
  assert.equal(fs.readFileSync(manifest, "utf8"), "previous release");
  fs.unlinkSync(path.join(dir, LAYOUT["core-node"].file));
  assert.throws(() => prepareArtifacts(dir, ["game-runner", "core-node"], metadata), /ENOENT/);
  assert.equal(fs.readFileSync(manifest, "utf8"), "previous release");
});

test("published locations preserve Pumpkin and match the desktop runner URL", (t) => {
  const artifacts = prepareArtifacts(fixture(t), ["pumpkin", "game-runner", "core-node"], metadata);
  assert.deepEqual(artifacts.map((a) => a.manifestKey), ["pumpkin/latest.json", "game-runner/latest.json", "core/latest.json"]);
  for (const artifact of artifacts) assert.equal(artifact.manifest.url, `${PUBLIC_BASE}/${artifact.objectKey}`);
  assert.match(artifacts[0].objectKey, /^pumpkin\/homerun-desktop-minecraft-pumpkin-[a-f0-9]{12}\.exe$/);
  assert.match(artifacts[1].objectKey, /^game-runner\/homerun-game-[a-f0-9]{12}\.exe$/);
  assert.match(artifacts[2].objectKey, /^core\/homerun-core-[a-f0-9]{12}\.node$/);
  assert.equal(artifacts[2].manifest.abi, 1);
  assert.equal(artifacts[2].manifest.sourceBuild, "fixture-revision");
});

test("an addon without ABI metadata cannot be offered to the desktop", (t) => {
  assert.throws(() => prepareArtifacts(fixture(t), ["core-node"], { "core-node": { version: "0.1.0" } }), /must export its ABI/);
});

test("runner can be prepared without building Pumpkin and targets Windows with static CRT", (t) => {
  const dir = fixture(t);
  fs.unlinkSync(path.join(dir, LAYOUT.pumpkin.file));
  assert.equal(prepareArtifacts(dir, ["game-runner"], metadata).length, 1);
  assert.equal(TARGETS["game-runner"].triple, "x86_64-pc-windows-msvc");
  assert.equal(TARGETS["game-runner"].requiresWindows, true);
  assert.equal(TARGETS["game-runner"].staticCrt, true);
  assert.equal(TARGETS["game-runner"].artifact, "homerun-game.exe");
});

function runPublisher(t, args, includeRunner = false) {
  const root = fixture(t);
  const dir = path.join(root, "dist", "desktop");
  fs.mkdirSync(dir, { recursive: true });
  fs.copyFileSync(path.join(root, LAYOUT.pumpkin.file), path.join(dir, LAYOUT.pumpkin.file));
  fs.copyFileSync(path.join(root, LAYOUT["core-node"].file), path.join(dir, LAYOUT["core-node"].file));
  if (includeRunner) {
    fs.copyFileSync(path.join(root, LAYOUT["game-runner"].file), path.join(dir, LAYOUT["game-runner"].file));
    const runnerCrate = path.join(root, "rust", "homerun-game-cli");
    fs.mkdirSync(runnerCrate, { recursive: true });
    fs.writeFileSync(path.join(runnerCrate, "Cargo.toml"), 'version = "0.1.0"');
  }
  const crate = path.join(root, "rust", "homerun-pumpkin-bin");
  fs.mkdirSync(crate, { recursive: true });
  fs.writeFileSync(path.join(crate, "Cargo.toml"), 'rev = "123456789abcdef"');
  const entry = { exports: {} };
  let queries = 0;
  function imports(name) {
    if (name === "./targets") return { ROOT: root };
    if (name === path.join(dir, LAYOUT["core-node"].file)) return {
      coreVersion: () => "0.1.0", coreAbiVersion: () => 1, coreBuildId: () => "fixture-revision",
    };
    if (name === "child_process") return { execFileSync(file, args, options) {
      assert.equal(file, path.join(dir, LAYOUT.pumpkin.file));
      assert.equal(args.join(" "), "--minecraft-version");
      assert.notEqual(options.cwd, root);
      queries++;
      return '{"minecraftVersion":"1.21.11","protocol":774}\n';
    } };
    return require(name);
  }
  imports.main = entry;
  const processStub = { argv: ["node", "publish", ...args], exitCode: 0 };
  require("node:vm").runInNewContext(fs.readFileSync(path.join(__dirname, "publish-desktop-artifacts.js"), "utf8"), {
    require: imports, module: entry, process: processStub, console: { log() {}, error() {} },
  });
  return { dir, queries, exitCode: processStub.exitCode };
}

test("Pumpkin publication still asks the built engine for its Minecraft version", (t) => {
  const { dir, queries, exitCode } = runPublisher(t, ["--only", "pumpkin"]);
  assert.equal(exitCode, 0);
  assert.equal(queries, 1, "publication must query the actual Pumpkin binary");
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT.pumpkin.manifest)));
  assert.equal(manifest.minecraftVersion, "1.21.11", "the desktop must retain the version used to choose a compatible client");
  assert.equal(manifest.protocol, 774);
  assert.equal(manifest.rev, "123456789abcdef");
});

test("the existing bare publish command works without a built runner", (t) => {
  const { dir, exitCode } = runPublisher(t, []);
  assert.equal(exitCode, 0, "existing Pumpkin/addon release jobs must not require a new build step");
  for (const kind of ["pumpkin", "core-node"]) assert.ok(fs.existsSync(path.join(dir, LAYOUT[kind].manifest)));
  assert.equal(fs.existsSync(path.join(dir, LAYOUT["game-runner"].manifest)), false);
});

test("bare publication includes an available runner but explicit selection refuses a missing runner", (t) => {
  const present = runPublisher(t, [], true);
  assert.equal(present.exitCode, 0);
  assert.ok(fs.existsSync(path.join(present.dir, LAYOUT["game-runner"].manifest)));
  const missing = runPublisher(t, ["--only", "game-runner"]);
  assert.equal(missing.exitCode, 1);
});

test("Pumpkin metadata cannot be omitted by direct preparation callers", (t) => {
  assert.throws(() => prepareArtifacts(fixture(t), ["pumpkin"]), /Minecraft version and protocol/);
});
