#!/usr/bin/env node
/*
  Does a Windows binary we ship need a Visual C++ runtime DLL?

  It must not. `VCRUNTIME140.dll` and its siblings come from the Visual C++
  Redistributable, which is not part of Windows. Plenty of PCs have it because
  some game or tool installed it; plenty do not. On one that does not, Windows
  refuses to load the binary at all:

  - The Pumpkin engine exits with 0xC0000135 (STATUS_DLL_NOT_FOUND) before
    `main`, so the desktop shows a crashed server with an empty log.
  - The core addon fails `require`, which takes Pumpkin and over-the-air UI
    updates down with it.

  That is what beta.34 shipped, and a player hit it on 2026-09-15. Nothing in
  the build said so, because every machine that built or tested it had the
  redistributable installed. The runtime is linked in now (`staticCrt` in
  targets.js); this reads the import table of the artifact itself, so a
  dependency that brings the DLL back fails the build on a machine that has it.

  The Universal CRT (`api-ms-win-crt-*`, `ucrtbase.dll`) is not on the list:
  it ships with Windows 10, which is the oldest Windows the desktop supports.

  A binary that carries the DLLs beside it is fine, and would fail this check
  anyway: the bundled JDK's `java.exe` imports VCRUNTIME140.dll and loads
  because `bin/` has its own copy. Only point this at binaries that ship alone.

    node scripts/check-windows-runtime.js dist/desktop/homerun_core.node
*/
const fs = require("fs");

/** DLLs that only the Visual C++ Redistributable installs. Lower-case. */
const REDISTRIBUTABLE_DLL = /^(vcruntime|msvcp|vccorlib|concrt|vcomp|vcamp)\d/;

/**
 * Every DLL a PE file names in its import and delay-load tables.
 *
 * Throws on a file that is not a PE image, rather than returning an empty list
 * that would read as "needs nothing".
 */
function peImports(buf) {
  if (buf.length < 0x40 || buf.toString("latin1", 0, 2) !== "MZ") {
    throw new Error("not a Windows executable (no MZ header)");
  }
  const pe = buf.readUInt32LE(0x3c);
  if (buf.toString("latin1", pe, pe + 4) !== "PE\0\0") {
    throw new Error("not a Windows executable (no PE signature)");
  }
  const sectionCount = buf.readUInt16LE(pe + 6);
  const optionalSize = buf.readUInt16LE(pe + 20);
  const optional = pe + 24;
  const is64 = buf.readUInt16LE(optional) === 0x20b;
  const dataDirectories = optional + (is64 ? 112 : 96);

  const sections = [];
  for (let i = 0; i < sectionCount; i++) {
    const s = optional + optionalSize + i * 40;
    sections.push({
      va: buf.readUInt32LE(s + 12),
      size: Math.max(buf.readUInt32LE(s + 8), buf.readUInt32LE(s + 16)),
      raw: buf.readUInt32LE(s + 20),
    });
  }
  const offsetOf = (rva) => {
    const s = sections.find((x) => rva >= x.va && rva < x.va + x.size);
    if (!s) throw new Error(`RVA 0x${rva.toString(16)} is in no section`);
    return rva - s.va + s.raw;
  };
  const cString = (at) => buf.toString("latin1", at, buf.indexOf(0, at));

  // Directory 1 is the import table (20-byte descriptors, name RVA at +12);
  // 13 is delay-load (32-byte descriptors, name RVA at +4). Both end in an
  // all-zero descriptor.
  const names = [];
  for (const [index, entrySize, nameAt] of [[1, 20, 12], [13, 32, 4]]) {
    const rva = buf.readUInt32LE(dataDirectories + index * 8);
    if (!rva) continue;
    for (let at = offsetOf(rva); ; at += entrySize) {
      const name = buf.readUInt32LE(at + nameAt);
      if (!name) break;
      names.push(cString(offsetOf(name)));
    }
  }
  return names;
}

/** The redistributable DLLs `buf` imports. Empty means it loads on a bare Windows. */
function redistributableImports(buf) {
  return peImports(buf).filter((dll) => REDISTRIBUTABLE_DLL.test(dll.toLowerCase()));
}

module.exports = { peImports, redistributableImports };

if (require.main === module) {
  const files = process.argv.slice(2);
  if (!files.length) {
    console.error("usage: check-windows-runtime.js <exe-or-dll>...");
    process.exit(2);
  }
  let failed = false;
  for (const file of files) {
    const needs = redistributableImports(fs.readFileSync(file));
    if (needs.length) {
      failed = true;
      console.error(
        `${file} needs ${needs.join(", ")}, from the Visual C++ Redistributable.\n` +
          "  It will not load on a PC without it."
      );
    } else {
      console.log(`${file}: no Visual C++ Redistributable DLLs imported`);
    }
  }
  process.exit(failed ? 1 : 0);
}
