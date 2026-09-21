const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");
const {
  prepareArtifacts, verifyArtifacts, resolveChannel, LAYOUT, CHANNELS, DEFAULT_CHANNEL, CHANNEL_ENV,
} = require("./publish-desktop-artifacts");
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
  for (const artifact of artifacts) {
    assert.equal(artifact.manifest.url, `${CHANNELS[artifact.channel].public}/${artifact.objectKey}`);
  }
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

// `reuse` is a previous run's root, for the two-invocation sequences: prepare,
// then verify what that left on disk, the way the publish workflow does.
function runPublisher(t, args, includeRunner = false, env = {}, reuse = null) {
  const root = reuse ?? fixture(t);
  const dir = path.join(root, "dist", "desktop");
  if (!reuse) {
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
  }
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
  // The printed `aws s3 cp` commands are the other half of the channel: a
  // manifest under the dev prefix uploaded by a prod command is the same
  // accident from the other direction, so the tests read what was printed.
  const logs = [];
  const processStub = { argv: ["node", "publish", ...args], env, exitCode: 0 };
  require("node:vm").runInNewContext(fs.readFileSync(path.join(__dirname, "publish-desktop-artifacts.js"), "utf8"), {
    require: imports, module: entry, process: processStub,
    console: { log: (line) => logs.push(String(line)), error: (line) => logs.push(String(line)) },
  });
  return { root, dir, queries, exitCode: processStub.exitCode, output: logs.join("\n") };
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

// The production prefix with its trailing slash. Without the slash it is a
// substring of `homerun-desktop-dev` and would match dev output too, which is
// the one thing these tests exist to tell apart.
const PROD_KEY_PREFIX = "homerun-desktop/";

function manifestsWritten(dir) {
  return Object.values(LAYOUT)
    .map((layout) => path.join(dir, layout.manifest))
    .filter((file) => fs.existsSync(file))
    .map((file) => fs.readFileSync(file, "utf8"));
}

test("prod publishes exactly the keys and URLs it published before channels existed", (t) => {
  const { dir, exitCode, output } = runPublisher(t, ["--channel", "prod"], true);
  assert.equal(exitCode, 0);
  const pumpkin = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT.pumpkin.manifest)));
  const runner = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT["game-runner"].manifest)));
  const addon = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT["core-node"].manifest)));
  assert.equal(
    pumpkin.url,
    `https://fractal-homerun.s3.amazonaws.com/homerun-desktop/pumpkin/homerun-desktop-minecraft-pumpkin-${pumpkin.build}.exe`
  );
  assert.equal(
    runner.url,
    `https://fractal-homerun.s3.amazonaws.com/homerun-desktop/game-runner/homerun-game-${runner.build}.exe`
  );
  assert.equal(
    addon.url,
    `https://fractal-homerun.s3.amazonaws.com/homerun-desktop/core/homerun-core-${addon.build}.node`
  );
  for (const line of [
    `aws s3 cp "${path.join(dir, LAYOUT.pumpkin.file)}" s3://fractal-homerun/homerun-desktop/pumpkin/homerun-desktop-minecraft-pumpkin-${pumpkin.build}.exe`,
    `aws s3 cp "${path.join(dir, LAYOUT.pumpkin.manifest)}" s3://fractal-homerun/homerun-desktop/pumpkin/latest.json --cache-control no-cache`,
    `aws s3 cp "${path.join(dir, LAYOUT["core-node"].file)}" s3://fractal-homerun/homerun-desktop/assets/homerun_core.node`,
  ]) {
    assert.ok(output.includes(line), `prod output must still print: ${line}`);
  }
});

test("dev names the production prefix nowhere, in any manifest or command", (t) => {
  const { dir, exitCode, output } = runPublisher(t, [], true);
  assert.equal(exitCode, 0, "dev is the default, so a bare run is a dev run");
  const written = manifestsWritten(dir);
  assert.equal(written.length, 3);
  for (const manifest of written) {
    assert.ok(!manifest.includes(PROD_KEY_PREFIX), `a dev manifest must not name production:\n${manifest}`);
    assert.match(manifest, /homerun-desktop-dev\//);
  }
  assert.ok(!output.includes(PROD_KEY_PREFIX), `dev commands must not name production:\n${output}`);
  // Including the mutable alias, which has no manifest to give it away.
  assert.ok(output.includes("s3://fractal-homerun/homerun-desktop-dev/assets/homerun_core.node"));
});

test("the channel comes from the flag, then the environment, then dev", (t) => {
  assert.equal(resolveChannel(null, {}), "dev");
  assert.equal(DEFAULT_CHANNEL, "dev");
  assert.equal(resolveChannel(null, { [CHANNEL_ENV]: "prod" }), "prod");
  assert.equal(resolveChannel("dev", { [CHANNEL_ENV]: "prod" }), "dev", "an explicit flag wins");
  // A workflow input left blank arrives as the empty string, not as absent.
  assert.equal(resolveChannel(null, { [CHANNEL_ENV]: "" }), "dev");
  assert.equal(resolveChannel(null, { [CHANNEL_ENV]: "  prod  " }), "prod");
  const fromEnv = runPublisher(t, [], false, { [CHANNEL_ENV]: "prod" });
  assert.equal(fromEnv.exitCode, 0);
  assert.match(JSON.parse(fs.readFileSync(path.join(fromEnv.dir, LAYOUT.pumpkin.manifest))).url, /homerun-desktop\/pumpkin\//);
});

test("an unknown channel is refused rather than resolved", (t) => {
  assert.throws(() => resolveChannel("staging", {}), /Unknown channel: staging/);
  assert.throws(() => resolveChannel(null, { [CHANNEL_ENV]: "PROD" }), /Unknown channel: PROD/);
  assert.throws(() => prepareArtifacts(fixture(t), ["game-runner"], metadata, "staging"), /Unknown channel/);
  assert.throws(() => verifyArtifacts(fixture(t), ["game-runner"], "staging"), /Unknown channel/);
  const byFlag = runPublisher(t, ["--channel", "staging"]);
  assert.equal(byFlag.exitCode, 1);
  assert.match(byFlag.output, /Unknown channel: staging/);
  assert.equal(manifestsWritten(byFlag.dir).length, 0, "a refused channel must not write a manifest");
  const byEnv = runPublisher(t, [], false, { [CHANNEL_ENV]: "production" });
  assert.equal(byEnv.exitCode, 1);
  assert.equal(manifestsWritten(byEnv.dir).length, 0);
});

test("--verify refuses a manifest that was hashed before signing", (t) => {
  const dir = fixture(t);
  const [artifact] = prepareArtifacts(dir, ["game-runner"], metadata);
  assert.deepEqual(verifyArtifacts(dir, ["game-runner"]), [], "a freshly prepared manifest describes its file");
  fs.appendFileSync(artifact.file, " simulated Authenticode signature");
  const problems = verifyArtifacts(dir, ["game-runner"]);
  assert.equal(problems.length, 4, "digest, build, size and URL all move with the bytes");
  assert.match(problems[0], /manifest sha256 .* but homerun-game\.exe hashes to .*prepare manifests AFTER signing/);
  prepareArtifacts(dir, ["game-runner"], metadata);
  assert.deepEqual(verifyArtifacts(dir, ["game-runner"]), [], "re-preparing after signing settles it");
});

test("--verify refuses a manifest prepared for the other channel", (t) => {
  const dir = fixture(t);
  prepareArtifacts(dir, ["game-runner"], metadata, "dev");
  const problems = verifyArtifacts(dir, ["game-runner"], "prod");
  assert.equal(problems.length, 1);
  assert.match(problems[0], /is not the prod channel's/);
  assert.deepEqual(verifyArtifacts(dir, ["game-runner"], "dev"), []);
});

test("--verify reports a missing manifest or file rather than throwing", (t) => {
  const dir = fixture(t);
  assert.match(verifyArtifacts(dir, ["game-runner"])[0], /cannot read game-runner-latest\.json/);
  prepareArtifacts(dir, ["game-runner"], metadata);
  fs.unlinkSync(path.join(dir, LAYOUT["game-runner"].file));
  assert.match(verifyArtifacts(dir, ["game-runner"])[0], /cannot hash homerun-game\.exe/);
});

test("--verify runs through the CLI and exits nonzero on a mismatch", (t) => {
  const prepared = runPublisher(t, ["--only", "game-runner"], true);
  assert.equal(prepared.exitCode, 0);
  const passed = runPublisher(t, ["--only", "game-runner", "--verify"], true, {}, prepared.root);
  assert.equal(passed.exitCode, 0, "what the previous invocation prepared must verify");
  assert.match(passed.output, /Verified game-runner against their dev manifests/);
  const manifest = path.join(prepared.dir, LAYOUT["game-runner"].manifest);
  const good = JSON.parse(fs.readFileSync(manifest));
  fs.writeFileSync(manifest, `${JSON.stringify({ ...good, sha256: "0".repeat(64) }, null, 2)}\n`);
  const failed = runPublisher(t, ["--only", "game-runner", "--verify"], true, {}, prepared.root);
  assert.equal(failed.exitCode, 1);
  assert.match(failed.output, /manifest sha256 0{64}/);
});

test("the usage line refuses arguments it does not understand", (t) => {
  for (const args of [["--channel"], ["--only"], ["--channel", "dev", "--channel", "prod"], ["--publish"], ["--verify", "--verify"], ["--only", "everything"]]) {
    const run = runPublisher(t, args);
    assert.equal(run.exitCode, 1, `must refuse: ${args.join(" ")}`);
    assert.equal(manifestsWritten(run.dir).length, 0);
  }
});
