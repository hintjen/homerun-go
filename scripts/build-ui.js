#!/usr/bin/env node
/**
 * Stages the shared UI bundle into a platform's asset directory.
 *
 *   node scripts/build-ui.js ios
 *   node scripts/build-ui.js android
 *   node scripts/build-ui.js            # both
 *
 * Same contract as the desktop app: a build ships whatever the UI is
 * currently at, so this re-resolves the dependency first. npm rebuilds the
 * bundle via its `prepare` hook and rewrites package-lock.json with the
 * commit it got — commit that with the release, it is the record of what
 * shipped.
 *
 *   HOMERUN_UI_DIR=<path to out/>  stage that build instead; no refresh
 *   HOMERUN_UI_NO_UPDATE=1         keep the pinned commit (offline, or
 *                                  reproducing an old build)
 *
 * `npm update` is deliberately not used: it treats a git branch dependency
 * as already satisfied and will not refetch. Re-installing the spec is what
 * re-resolves the ref.
 */
const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const { ROOT, UI_DESTINATIONS } = require("./targets");

const PACKAGE_DIR = path.join(ROOT, "node_modules", "homerun-app-ui", "out");

const requested = process.argv.slice(2).filter((a) => !a.startsWith("-"));
const platforms = requested.length ? requested : Object.keys(UI_DESTINATIONS);

for (const p of platforms) {
  if (!UI_DESTINATIONS[p]) {
    console.error(
      `Unknown platform "${p}". Expected: ${Object.keys(UI_DESTINATIONS).join(", ")}`
    );
    process.exit(2);
  }
}

/** The commit the lockfile pins. Read from disk — `require` would cache it. */
function pinnedCommit() {
  try {
    const lock = JSON.parse(
      fs.readFileSync(path.join(ROOT, "package-lock.json"), "utf8")
    );
    const resolved = lock.packages?.["node_modules/homerun-app-ui"]?.resolved ?? "";
    const hash = resolved.split("#")[1];
    return hash ? hash.slice(0, 7) : "unknown";
  } catch {
    return "unknown";
  }
}

function refresh() {
  if (process.env.HOMERUN_UI_DIR) {
    console.log("Shared UI: HOMERUN_UI_DIR is set — using that build.");
    return;
  }
  if (process.env.HOMERUN_UI_NO_UPDATE === "1") {
    console.log(`Shared UI: update skipped, staying on ${pinnedCommit()}.`);
    return;
  }

  const spec = require(path.join(ROOT, "package.json")).dependencies?.[
    "homerun-app-ui"
  ];
  if (!spec) {
    console.warn("WARNING: homerun-app-ui is not a dependency — skipping refresh.");
    return;
  }

  const before = pinnedCommit();
  console.log(`Shared UI: re-resolving ${spec} ...`);
  try {
    execFileSync("npm", ["install", spec, "--no-audit", "--no-fund"], {
      cwd: ROOT,
      stdio: "inherit",
      shell: true,
    });
  } catch (err) {
    console.warn(
      `\nWARNING: could not refresh the shared UI (${err.message.split("\n")[0]}).\n` +
        `         Staging the installed bundle at ${before}, which may be stale.\n`
    );
    return;
  }

  const after = pinnedCommit();
  console.log(
    after === before
      ? `Shared UI: already current at ${after}.`
      : `Shared UI: updated ${before} -> ${after}. Commit package-lock.json.`
  );
}

function sourceDir() {
  const dir = process.env.HOMERUN_UI_DIR
    ? path.resolve(ROOT, process.env.HOMERUN_UI_DIR)
    : PACKAGE_DIR;

  // index.html is the export's marker — an empty or half-copied directory
  // would otherwise stage silently and the app would show a blank screen.
  if (!fs.existsSync(path.join(dir, "index.html"))) {
    console.error(
      `\nNo built UI bundle at:\n  ${dir}\n\n` +
        (process.env.HOMERUN_UI_DIR
          ? "That path came from HOMERUN_UI_DIR. Run `npm run build` in the\n" +
            "homerun-app-ui repo, or unset it to use the pinned dependency."
          : "Run `npm install` to fetch and build the pinned homerun-app-ui\n" +
            "dependency, or set HOMERUN_UI_DIR to a built `out/`.")
    );
    process.exit(1);
  }
  return dir;
}

/**
 * Whether a file from the UI build belongs on a device.
 *
 * Source maps do not. They are debugger artifacts — nothing reads them at
 * runtime, and no user has a debugger attached — and they dominate the bundle:
 * 46 MB of the 57 MB staged before this filter existed, in 64 files, all of
 * which shipped inside the APK and sat on every phone.
 *
 * They are dropped here rather than in `homerun-app-ui`, which legitimately
 * wants them in its own `out/` for debugging the UI on a desktop. This is the
 * packaging step; deciding what a device receives is its job.
 *
 * The saving is worth stating twice over, because it applies to both delivery
 * routes: ~46 MB off every store download, and it is the difference between a
 * ~15 MB and a ~3.5 MB OTA bundle (`plans/ota-updates.md`).
 */
function shipped(source) {
  return !source.endsWith(".js.map");
}

refresh();
const src = sourceDir();

for (const platform of platforms) {
  const dest = UI_DESTINATIONS[platform];
  // Replace rather than merge: a stale file from a previous UI version
  // would otherwise linger and get served.
  fs.rmSync(dest, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.cpSync(src, dest, { recursive: true, filter: shipped });
  console.log(`${platform.padEnd(8)} <- ${dest}`);
}

console.log(`\nStaged from ${src}`);
