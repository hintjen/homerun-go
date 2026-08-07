/**
 * Finding a JDK the Android Gradle Plugin can actually run on.
 *
 * AGP 8.7 supports JDK 17 through 21. Android Studio's bundled JBR is often
 * newer than that, and JAVA_HOME usually points at it — so the obvious
 * environment is the wrong one more often than not. The failure it produces
 * ("Unsupported class file major version") names nothing you can act on, so
 * both the build script and the doctor pick deliberately rather than trusting
 * JAVA_HOME.
 */
const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const JDK_MIN = 17;
const JDK_MAX = 21;

const exe = (name) => (process.platform === "win32" ? `${name}.exe` : name);

/** Major version of the JDK at `home`, or null if that is not one. */
function javaMajor(home) {
  if (!home) return null;
  const java = path.join(home, "bin", exe("java"));
  if (!fs.existsSync(java)) return null;
  const result = spawnSync(java, ["-version"], { encoding: "utf8" });
  // `java -version` prints to stderr. Always has, always will.
  const text = `${result.stderr || ""}${result.stdout || ""}`;
  const match = text.match(/version "(\d+)/);
  return match ? Number(match[1]) : null;
}

/** Every place worth looking, most explicit first. */
function candidates() {
  const found = [process.env.HOMERUN_JAVA_HOME, process.env.JAVA_HOME];
  const roots =
    process.platform === "win32"
      ? [
          path.join(process.env.USERPROFILE || "", "tools"),
          "C:\\Program Files\\Eclipse Adoptium",
          "C:\\Program Files\\Java",
          "C:\\Program Files\\Microsoft",
        ]
      : process.platform === "darwin"
        ? ["/Library/Java/JavaVirtualMachines"]
        : ["/usr/lib/jvm"];

  for (const dir of roots) {
    if (!dir || !fs.existsSync(dir)) continue;
    for (const entry of fs.readdirSync(dir)) {
      const full = path.join(dir, entry);
      found.push(full);
      // macOS bundles nest the real home two levels down.
      found.push(path.join(full, "Contents", "Home"));
    }
  }
  return found;
}

/**
 * The first usable JDK, or null. `seen` lists every JDK found regardless of
 * version, so a caller can say what it rejected instead of just "not found".
 */
function findJdk() {
  const seen = [];
  let match = null;
  for (const home of candidates()) {
    const major = javaMajor(home);
    if (major === null) continue;
    if (seen.some((s) => s.home === home)) continue;
    seen.push({ home, major });
    if (!match && major >= JDK_MIN && major <= JDK_MAX) match = { home, major };
  }
  return { jdk: match, seen };
}

const INSTALL_HINT =
  `The Android Gradle Plugin supports JDK ${JDK_MIN}-${JDK_MAX} only. Install one from\n` +
  "    https://adoptium.net/temurin/releases/?version=21\n" +
  "    then point HOMERUN_JAVA_HOME at it.";

module.exports = { JDK_MIN, JDK_MAX, findJdk, javaMajor, INSTALL_HINT };
