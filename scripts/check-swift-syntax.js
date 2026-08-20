#!/usr/bin/env node
/**
 * Parse every Swift file, on a machine with no Xcode.
 *
 * # What this is for, and what it is emphatically not
 *
 * The iOS host is written on Windows and Linux machines and compiled on a Mac,
 * sometimes days later — `shared/conformance/host-revisions.json` has recorded
 * "written to the repo's uncompiled-until-a-Mac convention" since revision 9.
 * That gap has one cheap thing that can be done about it and one expensive
 * thing that cannot.
 *
 * The cheap thing is here: `swiftc -parse` inside the official Swift Linux
 * image. Parsing does not resolve modules, so it works on every file in the
 * project — UIKit, WebKit and Security ones included — and it catches an
 * unbalanced brace, a malformed string interpolation, a bad generic clause.
 *
 * **It does not type-check, and a pass here means very little.** Linux Swift
 * ships swift-corelibs-foundation, which does not contain `NSException`,
 * `NSSetUncaughtExceptionHandler`, UIKit, WebKit or Security — so
 * `swiftc -typecheck` cannot run against this project at all, and the errors
 * that actually bite ("that method does not exist", "that type is wrong",
 * "that argument label changed") are exactly the ones it cannot see. Treat a
 * pass as "this is syntactically Swift", never as "this compiles".
 *
 * The expensive thing — a real `xcodebuild` on a macOS runner — is what would
 * actually close the gap. Nothing here substitutes for it.
 *
 * # Why Docker
 *
 * So the check runs identically on the machines the Swift is written on. It
 * needs Docker and about 2.5 GB for the image, which is why it is not part of
 * `npm test`.
 */
const { execFileSync, spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const IMAGE = "swift:5.9";
const ROOT = path.resolve(__dirname, "..");
const IOS = path.join(ROOT, "ios");

/** Docker on Windows wants a `/c/Users/...` style path, not `C:\Users\...`. */
function mountPath(p) {
  const win = /^([A-Za-z]):[\\/](.*)$/.exec(p);
  if (!win) return p;
  return `/${win[1].toLowerCase()}/${win[2].replace(/\\/g, "/")}`;
}

function have(cmd, args) {
  const r = spawnSync(cmd, args, { stdio: "ignore", shell: false });
  return r.status === 0;
}

if (!fs.existsSync(IOS)) {
  console.error(`\nNo ios/ directory at ${IOS}\n`);
  process.exit(1);
}

if (!have("docker", ["version"])) {
  console.error(
    "\nDocker is not available, and this check runs the Swift toolchain in a\n" +
      "container so it behaves the same on every machine.\n\n" +
      "  docker pull " + IMAGE + "\n"
  );
  process.exit(1);
}

const script = `
fail=0; n=0
for f in $(find . -name '*.swift' | sort); do
  n=$((n+1))
  out=$(swiftc -parse "$f" 2>&1)
  if [ -n "$out" ]; then
    fail=$((fail+1))
    echo "FAIL $f"
    echo "$out" | head -5
  fi
done
echo "PARSED $n $fail"
`;

let output;
try {
  output = execFileSync(
    "docker",
    [
      "run", "--rm",
      "-v", `${mountPath(IOS)}:/src`,
      "-w", "/src",
      IMAGE,
      "sh", "-c", script,
    ],
    { encoding: "utf8", env: { ...process.env, MSYS_NO_PATHCONV: "1" } }
  );
} catch (err) {
  console.error(`\nCould not run ${IMAGE}:\n${err.message}\n`);
  process.exit(1);
}

const summary = /PARSED (\d+) (\d+)/.exec(output);
const detail = output.replace(/PARSED \d+ \d+\s*$/, "").trim();
if (detail) console.error(detail);

if (!summary) {
  console.error("\nThe container produced no summary line — treating as a failure.\n");
  process.exit(1);
}

const [, total, failed] = summary;

if (Number(failed) > 0) {
  console.error(`\nFAIL — ${failed} of ${total} Swift file(s) do not parse.\n`);
  process.exit(1);
}

console.log(
  `\n  ok    ${total} Swift file(s) parse\n\n` +
    "PASS — syntax only. This does NOT type-check: Linux Swift has no UIKit,\n" +
    "WebKit, Security or NSException, so a missing method or a changed argument\n" +
    "label still gets through. Only a build on a Mac catches those.\n"
);
