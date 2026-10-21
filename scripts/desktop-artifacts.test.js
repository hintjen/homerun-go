const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");
const {
  prepareArtifacts, verifyArtifacts, resolveChannel, resolveTarget, fileFor, LAYOUT, CHANNELS, DEFAULT_CHANNEL, CHANNEL_ENV,
  TARGET_KINDS,
} = require("./publish-desktop-artifacts");
const { TARGETS } = require("./targets");

const metadata = {
  pumpkin: { minecraftVersion: "1.21.11", protocol: 774 },
  "game-runner": { version: "0.1.0", protocol: 1, features: ["runtime-mounts"] },
  "core-node": { version: "0.1.0", abi: 1, sourceBuild: "fixture-revision" },
  wireproxy: { version: "006916920fc5", rev: "006916920fc50f7eadca3976b156ecfe39dfe88f" },
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
function runPublisher(t, args, includeRunner = false, env = {}, reuse = null, runnerAnswer = ["runtime-mounts"]) {
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
      assert.notEqual(options.cwd, root);
      queries++;
      // The runner is asked a different question, of a different file.
      if (file === path.join(dir, LAYOUT["game-runner"].file)) {
        assert.equal(args.join(" "), "--features");
        return `${JSON.stringify(runnerAnswer)}\n`;
      }
      assert.equal(file, path.join(dir, LAYOUT.pumpkin.file));
      assert.equal(args.join(" "), "--minecraft-version");
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

test("a runner manifest names the features the built binary reports", (t) => {
  // Asked of the artifact, never of the source tree: a manifest claiming a
  // capability the binary beside it does not have is exactly how a desktop
  // would decide it is safe to hand that build a descriptor it will mishandle.
  const { dir, exitCode } = runPublisher(t, ["--only", "game-runner"], true);
  assert.equal(exitCode, 0);
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT["game-runner"].manifest)));
  assert.deepEqual(manifest.features, ["runtime-mounts"]);
  assert.equal(manifest.protocol, 1);
  assert.equal(manifest.version, "0.1.0");
});

test("a runner answering --features with something else is not published", (t) => {
  for (const answer of [
    { runtimeMounts: true }, ["Runtime-Mounts"], ["ok", 7], "runtime-mounts",
    ["extension:"], [":hytale"], ["extension:hytale:more"], ["extension:Hytale"],
  ]) {
    const { dir, exitCode } = runPublisher(t, ["--only", "game-runner"], true, {}, null, answer);
    assert.equal(exitCode, 1, `${JSON.stringify(answer)} must not become a manifest`);
    assert.equal(fs.existsSync(path.join(dir, LAYOUT["game-runner"].manifest)), false);
  }
});

test("a runner's extension features are published as it names them", (t) => {
  // The first build with a compiled-in extension was refused here, because
  // this rule predated the `extension:<name>` token and had no colon in it.
  const answer = ["runtime-mounts", "extensions", "extension:hytale"];
  const { dir, exitCode } = runPublisher(t, ["--only", "game-runner"], true, {}, null, answer);
  assert.equal(exitCode, 0);
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT["game-runner"].manifest)));
  assert.deepEqual(manifest.features, answer);
});

test("a runner with no features publishes an empty list, not a missing one", (t) => {
  const { dir, exitCode } = runPublisher(t, ["--only", "game-runner"], true, {}, null, []);
  assert.equal(exitCode, 0);
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, LAYOUT["game-runner"].manifest)));
  assert.deepEqual(manifest.features, []);
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

// ── Targets other than Windows: one keyed manifest per target ─────────────────

/** A target's built artifacts, named as they are built for it. */
function targetFixture(t, target) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `homerun-artifacts-${target}-`));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  for (const kind of TARGET_KINDS[target]) {
    fs.writeFileSync(path.join(dir, fileFor(LAYOUT[kind], target)), `${target} ${kind}`);
  }
  return dir;
}

test("a Linux target is keyed: its manifest and its object both name the target", (t) => {
  const artifacts = prepareArtifacts(targetFixture(t, "linux-x64"), ["core-node", "wireproxy"], metadata, "dev", "linux-x64");
  assert.deepEqual(artifacts.map((a) => a.manifestKey), ["core/linux-x64/latest.json", "wireproxy/linux-x64/latest.json"]);
  assert.match(artifacts[0].objectKey, /^core\/linux-x64\/homerun-core-[a-f0-9]{12}\.node$/);
  // An executable off Windows has no extension, in its file or its key.
  assert.match(artifacts[1].objectKey, /^wireproxy\/linux-x64\/homerun-desktop-server-proxy-[a-f0-9]{12}$/);
  assert.equal(path.basename(artifacts[1].file), "homerun-desktop-server-proxy");
  for (const artifact of artifacts) {
    assert.equal(artifact.manifest.url, `${CHANNELS.dev.public}/${artifact.objectKey}`);
    assert.equal(artifact.target, "linux-x64");
  }
  assert.equal(artifacts[1].manifest.rev, metadata.wireproxy.rev);
});

test("Windows stays unkeyed, wireproxy included, so released desktops see no change", (t) => {
  const [wireproxy] = prepareArtifacts(fixture(t), ["wireproxy"], metadata, "prod");
  assert.equal(wireproxy.manifestKey, "wireproxy/latest.json");
  assert.match(wireproxy.objectKey, /^wireproxy\/homerun-desktop-server-proxy-[a-f0-9]{12}\.exe$/);
});

test("the runner and Pumpkin for linux-x64 are keyed executables, with no .exe", (t) => {
  const artifacts = prepareArtifacts(targetFixture(t, "linux-x64"), ["game-runner", "pumpkin"], metadata, "dev", "linux-x64");
  assert.deepEqual(artifacts.map((a) => a.manifestKey), ["game-runner/linux-x64/latest.json", "pumpkin/linux-x64/latest.json"]);
  assert.match(artifacts[0].objectKey, /^game-runner\/linux-x64\/homerun-game-[a-f0-9]{12}$/);
  assert.match(artifacts[1].objectKey, /^pumpkin\/linux-x64\/homerun-desktop-minecraft-pumpkin-[a-f0-9]{12}$/);
  assert.deepEqual(artifacts.map((a) => path.basename(a.file)), ["homerun-game", "homerun-desktop-minecraft-pumpkin"]);
  assert.deepEqual(artifacts[0].manifest.features, metadata["game-runner"].features);
});

// The publish itself, not prepareArtifacts with metadata handed to it: what the
// runner and Pumpkin say about themselves is asked of the binaries, and off
// Windows those are named without `.exe`.
test("a linux-x64 publish asks the Linux binaries themselves, by their own names", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "homerun-publish-linux-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dir = path.join(root, "dist", "desktop", "linux-x64");
  fs.mkdirSync(dir, { recursive: true });
  for (const kind of ["game-runner", "pumpkin"]) fs.writeFileSync(path.join(dir, fileFor(LAYOUT[kind], "linux-x64")), `linux-x64 ${kind}`);
  for (const [crate, toml] of [["homerun-game-cli", 'version = "0.1.0"'], ["homerun-pumpkin-bin", 'rev = "123456789abcdef"']]) {
    fs.mkdirSync(path.join(root, "rust", crate), { recursive: true });
    fs.writeFileSync(path.join(root, "rust", crate, "Cargo.toml"), toml);
  }
  const asked = [];
  function imports(name) {
    if (name === "./targets") return { ROOT: root };
    if (name === "child_process") return { execFileSync(file, args) {
      asked.push(`${path.basename(file)} ${args.join(" ")}`);
      return args[0] === "--features" ? '["runtime-mounts"]\n' : '{"minecraftVersion":"26.2","protocol":776}\n';
    } };
    return require(name);
  }
  for (const kind of ["game-runner", "pumpkin"]) {
    const entry = { exports: {} };
    imports.main = entry;
    const processStub = { argv: ["node", "publish", "--target", "linux-x64", "--only", kind], env: {}, exitCode: 0, platform: "linux", arch: "x64" };
    require("node:vm").runInNewContext(fs.readFileSync(path.join(__dirname, "publish-desktop-artifacts.js"), "utf8"), {
      require: imports, module: entry, process: processStub, console: { log() {}, error() {} },
    });
    assert.equal(processStub.exitCode, 0, `${kind} for linux-x64 must publish`);
  }
  assert.deepEqual(asked, ["homerun-game --features", "homerun-desktop-minecraft-pumpkin --minecraft-version"]);
  assert.equal(JSON.parse(fs.readFileSync(path.join(dir, LAYOUT.pumpkin.manifest))).minecraftVersion, "26.2");
});

test("an artifact a target does not have yet is refused, not published under a key nothing reads", (t) => {
  const dir = targetFixture(t, "linux-arm64");
  fs.writeFileSync(path.join(dir, "homerun-desktop-minecraft-pumpkin"), "an arm64 engine nobody reads");
  assert.throws(() => prepareArtifacts(dir, ["pumpkin"], metadata, "dev", "linux-arm64"), /There is no pumpkin for linux-arm64 yet/);
  assert.throws(() => prepareArtifacts(targetFixture(t, "linux-arm64"), ["core-node"], metadata, "dev", "linux-arm64"), /no core-node for linux-arm64/);
});

test("darwin-arm64 has all four, keyed like linux-x64", (t) => {
  const artifacts = prepareArtifacts(targetFixture(t, "darwin-arm64"), ["core-node", "wireproxy"], metadata, "dev", "darwin-arm64");
  assert.deepEqual(artifacts.map((a) => a.manifestKey), ["core/darwin-arm64/latest.json", "wireproxy/darwin-arm64/latest.json"]);
  assert.match(artifacts[0].objectKey, /^core\/darwin-arm64\/homerun-core-[a-f0-9]{12}\.node$/);
  assert.deepEqual(TARGET_KINDS["darwin-arm64"], TARGET_KINDS["linux-x64"]);
});

test("an unknown target is refused rather than resolved", () => {
  assert.equal(resolveTarget(undefined), "win32-x64");
  assert.throws(() => resolveTarget("linux-x86_64"), /Unknown target: linux-x86_64/);
  assert.throws(() => resolveTarget("darwin-x64"), /Unknown target/, "no Intel Macs");
});

test("wireproxy must name the fork revision it was built from", (t) => {
  assert.throws(
    () => prepareArtifacts(targetFixture(t, "linux-arm64"), ["wireproxy"], { wireproxy: { version: "x" } }, "dev", "linux-arm64"),
    /must name the fork revision/
  );
});

test("--verify refuses a manifest prepared for another target", (t) => {
  const dir = targetFixture(t, "linux-x64");
  prepareArtifacts(dir, ["wireproxy"], metadata, "dev", "linux-x64");
  // Same bytes, same name, other target: the url names linux-x64, so arm64 must not accept it.
  fs.mkdirSync(path.join(dir, "arm"));
  fs.copyFileSync(path.join(dir, "homerun-desktop-server-proxy"), path.join(dir, "arm", "homerun-desktop-server-proxy"));
  fs.copyFileSync(path.join(dir, LAYOUT.wireproxy.manifest), path.join(dir, "arm", LAYOUT.wireproxy.manifest));
  assert.deepEqual(verifyArtifacts(dir, ["wireproxy"], "dev", "linux-x64"), []);
  const problems = verifyArtifacts(path.join(dir, "arm"), ["wireproxy"], "dev", "linux-arm64");
  assert.equal(problems.length, 1);
  assert.match(problems[0], /is not the dev channel's linux-arm64/);
});
