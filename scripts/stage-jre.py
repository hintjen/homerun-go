#!/usr/bin/env python3
"""Stage the Java runtimes into the Android assets, at build time.

    python3 scripts/stage-jre.py arm64-v8a
    python3 scripts/stage-jre.py x86_64 --java 21
    python3 scripts/stage-jre.py arm64-v8a --java 21,25

Google Play forbids an app downloading executable code — "dex, JAR, .so files"
— from anywhere but Play itself, so the runtime cannot be fetched at first run.
It ships inside the app instead, and this puts it there.

Everything except the launcher goes in `assets/`, not `jniLibs/`: jniLibs only
packages files ending in `.so`, which silently drops `libz.so.1` (a versioned
soname the linker asks for by name) and flattens a tree the JVM expects to walk
from `java.home`. Assets keep the layout intact. Only `libjavabin.so` has to be
in jniLibs, because it is the one thing that gets `exec`'d.

# Why more than one

Minecraft needs a Java at least as new as the version it names; mod loaders
need one that is *exactly* right, because modlauncher breaks on JDKs newer than
it was built against. One runtime cannot serve both, so this stages a directory
per major — `assets/jre-21/`, `assets/jre-25/` — and `homerun-core` picks which
one a given server launches on. See `plans/android-mod-loaders.md`.

Each directory is **self-contained**: its own `termux-lib/`, its own
`java-major`, its own `release`. The duplicated dependency libraries cost about
1.6 MB per runtime, which buys a `java.home` the host can point at without any
cross-runtime path fixing — and a runtime it can unpack on its own, without
dragging the other one out of the APK with it.

Needs only the Python standard library — no `ar`, `xz` or `tar` binary, none of
which is reliably present on Windows.
"""
import argparse
import glob
import io
import lzma
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import urllib.request

REPO = "https://packages.termux.dev/apt/termux-main/pool/main"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSET_ROOT = os.path.join(ROOT, "android", "app", "src", "main", "assets")
CACHE = os.path.join(ROOT, ".jre-cache")

# Pinned: an upstream bump must not silently change what ships.
JDK_VERSIONS = {17: "17.0.20", 21: "21.0.12", 25: "25.0.4"}

# What a release ships, and what `verifyJavaRuntime` in app/build.gradle.kts
# insists on. 25 runs the current Minecraft release; 21 is what the mod loaders
# want, and running them on 25 is not an upgrade but a failure. 17 would unlock
# Forge 1.20.1 and is deliberately not here — see `plans/android-mod-loaders.md`
# for what that costs and why the answer is still two.
DEFAULT_JAVA = [21, 25]

# One staged runtime lives in `assets/jre-<major>/`. The host lists the asset
# root to discover them, so the prefix is load-bearing on both sides.
ASSET_PREFIX = "jre-"

# Derived by reading DT_NEEDED across the whole closure, not guessed. libc++ is
# the one a scan of only the JRE's own libraries misses — libandroid-spawn.so
# needs it, and without it the VM will not load.
#
# libandroid-spawn is NOT here. Termux's published 0.3 binary is 4 KB-page
# aligned and there is nothing newer, so it is built from vendored source
# instead — see `build_android_spawn` and third_party/libandroid-spawn/README.md.
DEPENDENCIES = [
    ("liba/libandroid-shmem", "libandroid-shmem", "0.7"),
    ("z/zlib", "zlib", "1.3.2"),
    ("libc/libc++", "libc++", "29"),
]

DEPS_DIR = "termux-lib"

# Android's dynamic linker refuses a library whose LOAD segments are aligned
# more coarsely than the kernel's page size, and 16 KB pages are the default on
# new 64-bit hardware. A 4 KB-only object therefore does not fail at build time
# or install time — it fails at `dlopen` on someone's phone.
MIN_PAGE_ALIGN = 16 * 1024

SPAWN_SRC = os.path.join(ROOT, "third_party", "libandroid-spawn")

# The NDK API level to build it against. 26 matches the app's minSdk, so the
# library works everywhere the app installs — including the API 26/27 devices
# where Bionic has no posix_spawn of its own and this is not merely a shim.
SPAWN_API = 26

# Termux ships a full JDK. A server needs a runtime, and the difference is
# ~85 MB of things that never execute here. `legal/` stays — these are
# GPLv2+CE builds and the notices ship with them.
PRUNE = [
    ("jmods", "jlink input; we never re-link the runtime on device"),
    ("demo", "sample code"),
    ("man", "manual pages"),
    ("include", "JNI headers, for compiling against the JDK"),
    ("lib/ct.sym", "javac --release history"),
    ("lib/src.zip", "class library sources"),
]


def fetch(url: str) -> str:
    """Download to the cache, or reuse what is already there."""
    os.makedirs(CACHE, exist_ok=True)
    path = os.path.join(CACHE, os.path.basename(url))
    if os.path.exists(path) and os.path.getsize(path) > 0:
        print(f"  cached  {os.path.basename(url)}")
        return path
    print(f"  fetch   {url}")
    with urllib.request.urlopen(url) as response, open(path, "wb") as out:
        shutil.copyfileobj(response, out)
    return path


def data_member(deb_path: str) -> bytes:
    """A .deb is an `ar` archive; the payload is its data.tar.* member."""
    with open(deb_path, "rb") as fh:
        if fh.read(8) != b"!<arch>\n":
            raise SystemExit(f"{deb_path} is not a .deb")
        while True:
            header = fh.read(60)
            if len(header) < 60:
                break
            name = header[0:16].decode().strip().rstrip("/")
            size = int(header[48:58].decode().strip())
            payload = fh.read(size)
            if size % 2:
                fh.read(1)
            if name.startswith("data.tar"):
                return lzma.decompress(payload) if name.endswith(".xz") else payload
    raise SystemExit(f"{deb_path} has no data.tar member")


def unpack(deb_path: str, dest: str, strip: str) -> int:
    """Extract one .deb, discarding `strip` from the front of every path.

    Symlinks are materialised as real copies. zlib ships `libz.so.1` as a link
    to the real file and that is exactly the soname `libzip.so` asks for, so
    dropping links leaves the JVM unable to read a jar — and preserving them as
    links fails on Windows, where this may well be built.
    """
    raw = data_member(deb_path)
    prefix = strip.strip("/")
    links: list[tuple[str, str]] = []
    count = 0

    with tarfile.open(fileobj=io.BytesIO(raw)) as tar:
        for entry in tar:
            name = entry.name.lstrip("./").lstrip("/")
            if not name.startswith(prefix):
                continue
            relative = name[len(prefix):].lstrip("/")
            if not relative:
                continue
            out = os.path.join(dest, relative)
            if os.path.commonpath([os.path.abspath(out), os.path.abspath(dest)]) != os.path.abspath(dest):
                raise SystemExit(f"archive entry escapes the target: {entry.name}")

            if entry.isdir():
                os.makedirs(out, exist_ok=True)
            elif entry.issym() or entry.islnk():
                links.append((out, entry.linkname))
            else:
                os.makedirs(os.path.dirname(out), exist_ok=True)
                source = tar.extractfile(entry)
                if source is None:
                    continue
                with open(out, "wb") as fh:
                    shutil.copyfileobj(source, fh)
                count += 1

    for out, target in links:
        resolved = os.path.normpath(os.path.join(os.path.dirname(out), target))
        if os.path.isfile(resolved):
            os.makedirs(os.path.dirname(out), exist_ok=True)
            shutil.copyfile(resolved, out)
            count += 1
    return count


def ndk_tool(name: str) -> str:
    """
    An NDK toolchain binary, by name, or exit saying how to get one.

    `.cmd` first: on Windows the NDK ships both an extensionless wrapper and a
    `.cmd`, and only the latter is executable from a plain subprocess call.
    """
    root = os.environ.get("ANDROID_NDK_HOME") or os.environ.get("ANDROID_NDK_ROOT")
    if not root:
        sdk = os.environ.get("ANDROID_HOME") or os.environ.get("ANDROID_SDK_ROOT")
        found = sorted(glob.glob(os.path.join(sdk, "ndk", "*"))) if sdk else []
        root = found[-1] if found else None
    if not root:
        raise SystemExit(
            "The Android NDK is needed to build libandroid-spawn.\n"
            "Set ANDROID_NDK_HOME, or install it from Android Studio > SDK Manager > NDK."
        )
    hosts = ["windows-x86_64", "linux-x86_64", "darwin-x86_64"]
    for host in hosts:
        base = os.path.join(root, "toolchains", "llvm", "prebuilt", host, "bin")
        for candidate in (name + ".cmd", name + ".exe", name):
            path = os.path.join(base, candidate)
            if os.path.isfile(path):
                return path
    raise SystemExit(f"No {name} in the NDK at {root}")


def load_alignments(path: str) -> list:
    """
    The p_align of every PT_LOAD segment in an ELF64 file, or [] if not one.

    Hand-rolled for the same reason `build-restic.js` walks program headers by
    hand: there is no readelf we can rely on across the three host platforms
    this script runs on, and the header layout is fixed.
    """
    with open(path, "rb") as fh:
        head = fh.read(64)
        if len(head) < 64 or head[:4] != b"\x7fELF" or head[4] != 2:
            return []  # not ELF, or 32-bit — nothing here produces those
        phoff = struct.unpack_from("<Q", head, 32)[0]
        phentsize, phnum = struct.unpack_from("<HH", head, 54)
        fh.seek(phoff)
        table = fh.read(phentsize * phnum)

    aligns = []
    for i in range(phnum):
        entry = table[i * phentsize : (i + 1) * phentsize]
        if len(entry) < 56 or struct.unpack_from("<I", entry, 0)[0] != 1:  # PT_LOAD
            continue
        aligns.append(struct.unpack_from("<Q", entry, 48)[0])
    return aligns


def verify_page_alignment(root: str) -> int:
    """
    Refuse to stage a runtime that cannot load on a 16 KB-page device.

    This exists because the failure it catches is invisible everywhere else:
    the build succeeds, the APK installs, the app runs, and only starting a
    server fails — with no exception and no output, because the thing that
    failed to load is the JVM itself. It cost a day to find once.
    """
    bad = []
    checked = 0
    for base, _dirs, files in os.walk(root):
        for name in files:
            path = os.path.join(base, name)
            if ".so" not in name:
                continue
            aligns = load_alignments(path)
            if not aligns:
                continue
            checked += 1
            worst = min(aligns)
            if worst < MIN_PAGE_ALIGN:
                bad.append((os.path.relpath(path, root), worst))

    if bad:
        lines = "\n".join(f"    {p}   aligned to {a} bytes" for p, a in bad)
        raise SystemExit(
            f"\n{len(bad)} staged librar{'y' if len(bad) == 1 else 'ies'} cannot load on a "
            f"16 KB-page device:\n\n{lines}\n\n"
            "Android's linker refuses a library aligned more coarsely than the page\n"
            "size, and 16 KB is the default on new 64-bit hardware. Google Play has\n"
            "required 16 KB support for targetSdk 35+ since 1 November 2025.\n\n"
            "Rebuild the offender with -Wl,-z,max-page-size=16384."
        )
    return checked


def build_android_spawn(abi: str, dest: str) -> None:
    """
    Build libandroid-spawn.so from the vendored source, 16 KB aligned.

    Built rather than downloaded because Termux publishes only a 4 KB-aligned
    0.3, and `libjvm.so` has a hard DT_NEEDED on this library — so on a 16 KB
    device the published binary stops the VM from loading at all. See
    third_party/libandroid-spawn/README.md.
    """
    triple = {"arm64-v8a": "aarch64", "x86_64": "x86_64"}[abi]
    cxx = ndk_tool(f"{triple}-linux-android{SPAWN_API}-clang++")
    obj = os.path.join(CACHE, f"posix_spawn-{abi}.o")
    out = os.path.join(dest, "libandroid-spawn.so")
    os.makedirs(CACHE, exist_ok=True)
    os.makedirs(dest, exist_ok=True)

    subprocess.run(
        [cxx, "-O2", "-fPIC", f"-I{SPAWN_SRC}", "-c",
         os.path.join(SPAWN_SRC, "posix_spawn.cpp"), "-o", obj],
        check=True,
    )
    subprocess.run(
        [cxx, "-shared", obj, "-o", out,
         # The whole point. Without it the NDK still emits 4 KB on some
         # toolchain versions, which is exactly how the shipped one got here.
         "-Wl,-z,max-page-size=16384",
         # The JRE's own libraries ask for it by this name, not by a path.
         "-Wl,-soname,libandroid-spawn.so"],
        check=True,
    )
    os.remove(obj)
    print(f"  build   libandroid-spawn.so ({os.path.getsize(out) / 1024:.0f} KB, 16 KB aligned)")


def stage_one(abi: str, java: int) -> tuple[int, int]:
    """Stage one runtime into its own asset directory. Returns (files, bytes)."""
    termux_abi = {"arm64-v8a": "aarch64", "x86_64": "x86_64"}[abi]
    version = JDK_VERSIONS[java]
    assets = os.path.join(ASSET_ROOT, f"{ASSET_PREFIX}{java}")

    print(f"\nStaging OpenJDK {version} ({abi}) into assets/{ASSET_PREFIX}{java}\n")

    if os.path.isdir(assets):
        shutil.rmtree(assets)
    os.makedirs(assets, exist_ok=True)

    jre_url = f"{REPO}/o/openjdk-{java}/openjdk-{java}_{version}_{termux_abi}.deb"
    total = unpack(
        fetch(jre_url),
        assets,
        f"data/data/com.termux/files/usr/lib/jvm/java-{java}-openjdk",
    )

    deps_dir = os.path.join(assets, DEPS_DIR)
    for path, name, dep_version in DEPENDENCIES:
        url = f"{REPO}/{path}/{name}_{dep_version}_{termux_abi}.deb"
        total += unpack(fetch(url), deps_dir, "data/data/com.termux/files/usr/lib")

    # Not from the pool — the published binary is 4 KB aligned and would stop
    # the VM loading on a 16 KB-page device.
    build_android_spawn(abi, deps_dir)
    total += 1

    freed = 0
    for relative, why in PRUNE:
        path = os.path.join(assets, *relative.split("/"))
        if not os.path.exists(path):
            continue
        if os.path.isdir(path):
            freed += sum(
                os.path.getsize(os.path.join(b, f))
                for b, _d, fs in os.walk(path) for f in fs
            )
            shutil.rmtree(path)
        else:
            freed += os.path.getsize(path)
            os.remove(path)
        print(f"  prune   {relative:<14} ({why})")
    if freed:
        print(f"  freed   {freed / 1024 / 1024:.0f} MB")

    # What the host reads to know which runtime it is holding. NOT dot-prefixed:
    # aapt's asset filter includes `.*`, so a hidden file is silently dropped
    # from the APK — the same trap as the `_next/` directory in the UI bundle.
    with open(os.path.join(assets, "java-major"), "w", encoding="utf-8") as fh:
        fh.write(str(java))

    libjvm = os.path.join(assets, "lib", "server", "libjvm.so")
    if not os.path.isfile(libjvm):
        raise SystemExit(f"staged, but there is no libjvm.so at {libjvm}")

    # Per runtime, not once at the end: each is unpacked and loaded on its own,
    # so each has to be able to load on its own.
    checked = verify_page_alignment(assets)
    print(f"  verify  {checked} shared objects, all >= 16 KB page aligned")

    size = sum(
        os.path.getsize(os.path.join(base, f))
        for base, _dirs, files in os.walk(assets)
        for f in files
    )
    print(f"  staged  {total} files, {size / 1024 / 1024:.0f} MB -> {assets}")
    return total, size


def requested_majors(raw: str) -> list[int]:
    """`21,25` -> [21, 25], rejecting anything not pinned in JDK_VERSIONS."""
    majors = []
    for piece in raw.split(","):
        piece = piece.strip()
        if not piece:
            continue
        try:
            major = int(piece)
        except ValueError:
            raise SystemExit(f"not a Java major version: {piece!r}")
        if major not in JDK_VERSIONS:
            known = ", ".join(str(v) for v in sorted(JDK_VERSIONS))
            raise SystemExit(f"no pinned OpenJDK {major}; this script knows {known}")
        if major not in majors:
            majors.append(major)
    if not majors:
        raise SystemExit("--java named no versions")
    return sorted(majors)


def drop_stale_runtimes(keep: list[int]) -> None:
    """
    Remove runtime directories this run did not stage.

    Without this, dropping a version from `--java` leaves the old directory in
    `assets/`, the host discovers it, and the app offers a runtime the build no
    longer intends to ship. Also clears the pre-multi-runtime `assets/jre/`.
    """
    wanted = {f"{ASSET_PREFIX}{major}" for major in keep}
    for name in sorted(os.listdir(ASSET_ROOT)):
        path = os.path.join(ASSET_ROOT, name)
        if not os.path.isdir(path):
            continue
        stale = name == "jre" or (name.startswith(ASSET_PREFIX) and name not in wanted)
        if stale:
            shutil.rmtree(path)
            print(f"  drop    assets/{name} (not in this build)")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("abi", choices=["arm64-v8a", "x86_64"])
    parser.add_argument(
        "--java",
        default=",".join(str(v) for v in DEFAULT_JAVA),
        help="comma-separated Java majors to stage (default: %(default)s)",
    )
    args = parser.parse_args()

    majors = requested_majors(args.java)
    os.makedirs(ASSET_ROOT, exist_ok=True)
    drop_stale_runtimes(majors)

    files = 0
    size = 0
    for major in majors:
        staged_files, staged_size = stage_one(args.abi, major)
        files += staged_files
        size += staged_size

    runtimes = ", ".join(f"Java {m}" for m in majors)
    print(f"\n{runtimes}: {files} files, {size / 1024 / 1024:.0f} MB total")
    print(f"Build with:  ./gradlew installDebug -Pabi={args.abi}\n")


if __name__ == "__main__":
    sys.exit(main())
