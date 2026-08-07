#!/usr/bin/env python3
"""Stage a Java runtime into the Android assets, at build time.

    python3 scripts/stage-jre.py arm64-v8a
    python3 scripts/stage-jre.py x86_64 --java 21

Google Play forbids an app downloading executable code — "dex, JAR, .so files"
— from anywhere but Play itself, so the runtime cannot be fetched at first run.
It ships inside the app instead, and this puts it there.

Everything except the launcher goes in `assets/`, not `jniLibs/`: jniLibs only
packages files ending in `.so`, which silently drops `libz.so.1` (a versioned
soname the linker asks for by name) and flattens a tree the JVM expects to walk
from `java.home`. Assets keep the layout intact. Only `libjavabin.so` has to be
in jniLibs, because it is the one thing that gets `exec`'d.

Needs only the Python standard library — no `ar`, `xz` or `tar` binary, none of
which is reliably present on Windows.
"""
import argparse
import io
import lzma
import os
import shutil
import sys
import tarfile
import urllib.request

REPO = "https://packages.termux.dev/apt/termux-main/pool/main"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "android", "app", "src", "main", "assets", "jre")
CACHE = os.path.join(ROOT, ".jre-cache")

# Pinned: an upstream bump must not silently change what ships.
JDK_VERSIONS = {17: "17.0.20", 21: "21.0.12", 25: "25.0.4"}

# Derived by reading DT_NEEDED across the whole closure, not guessed. libc++ is
# the one a scan of only the JRE's own libraries misses — libandroid-spawn.so
# needs it, and without it the VM will not load.
DEPENDENCIES = [
    ("liba/libandroid-shmem", "libandroid-shmem", "0.7"),
    ("liba/libandroid-spawn", "libandroid-spawn", "0.3"),
    ("z/zlib", "zlib", "1.3.2"),
    ("libc/libc++", "libc++", "29"),
]

DEPS_DIR = "termux-lib"

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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("abi", choices=["arm64-v8a", "x86_64"])
    # 25 by default: the current Minecraft release requires it, and it runs
    # older targets too.
    parser.add_argument("--java", type=int, default=25, choices=sorted(JDK_VERSIONS))
    args = parser.parse_args()

    termux_abi = {"arm64-v8a": "aarch64", "x86_64": "x86_64"}[args.abi]
    version = JDK_VERSIONS[args.java]

    print(f"\nStaging OpenJDK {version} ({args.abi}) into assets\n")

    if os.path.isdir(ASSETS):
        shutil.rmtree(ASSETS)
    os.makedirs(ASSETS, exist_ok=True)

    jre_url = f"{REPO}/o/openjdk-{args.java}/openjdk-{args.java}_{version}_{termux_abi}.deb"
    total = unpack(
        fetch(jre_url),
        ASSETS,
        f"data/data/com.termux/files/usr/lib/jvm/java-{args.java}-openjdk",
    )

    deps_dir = os.path.join(ASSETS, DEPS_DIR)
    for path, name, dep_version in DEPENDENCIES:
        url = f"{REPO}/{path}/{name}_{dep_version}_{termux_abi}.deb"
        total += unpack(fetch(url), deps_dir, "data/data/com.termux/files/usr/lib")

    freed = 0
    for relative, why in PRUNE:
        path = os.path.join(ASSETS, *relative.split("/"))
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
    with open(os.path.join(ASSETS, "java-major"), "w", encoding="utf-8") as fh:
        fh.write(str(args.java))

    libjvm = os.path.join(ASSETS, "lib", "server", "libjvm.so")
    if not os.path.isfile(libjvm):
        raise SystemExit(f"staged, but there is no libjvm.so at {libjvm}")

    size = sum(
        os.path.getsize(os.path.join(base, f))
        for base, _dirs, files in os.walk(ASSETS)
        for f in files
    )
    print(f"\n{total} files, {size / 1024 / 1024:.0f} MB -> {ASSETS}")
    print(f"Build with:  ./gradlew installDebug -Pabi={args.abi}\n")


if __name__ == "__main__":
    sys.exit(main())
