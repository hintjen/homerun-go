# libandroid-spawn

`posix_spawn` and friends for Android, vendored from Termux's package recipe
([`packages/libandroid-spawn`](https://github.com/termux/termux-packages/tree/master/packages/libandroid-spawn),
version 0.3, BSD 2-Clause — see `LICENSE`). Two files, unmodified.

## Why this is here rather than fetched as a `.deb`

`scripts/stage-jre.py` takes the JRE and its other dependencies from Termux's
apt pool as prebuilt `.deb`s. This one is built from source instead, because
**the published 0.3 binary is 4 KB-page aligned and nothing newer exists.**

That is not cosmetic. `libjvm.so` and `libjava.so` both carry
`DT_NEEDED: libandroid-spawn.so`, so on a device with 16 KB pages — a Pixel 9,
and increasingly the default on new hardware — the dynamic linker refuses the
library and **the JVM does not start at all**. The failure is silent from the
app's side: no JVM means no console output, which means no crash report, so
what a player sees is a server that downloads its jar and then does nothing.

Google Play has required 16 KB support for apps targeting Android 15+ since
1 November 2025 (extended to 31 May 2026), so this also blocks a production
release regardless of any particular phone.

Every other object staged into `assets/jre-<major>/` — including `libjvm.so`
itself, `libc++_shared.so` and zlib — is already 16 KB aligned. This was the
only one, and it is built once per staged runtime.

## What the JVM actually needs from it

Exactly one symbol. `libjvm.so` and `libjava.so` each leave `posix_spawn`
undefined and nothing else from this library:

```
llvm-readelf --dyn-syms libjvm.so | awk '$7=="UND"{print $NF}' | grep spawn
posix_spawn
```

Our build exports the full public `posix_spawn*` family. It exports three
fewer *internal* C++ symbols than Termux's binary (`ScopedSignalBlocker::…`,
`__posix_spawn_file_actions::Do`), which are implementation details of this
same translation unit and are referenced by nothing outside it.

## Updating

If Termux ever publishes a 16 KB-aligned build, this can go back to being a
`.deb` like the rest. Check with:

```sh
curl -s https://packages.termux.dev/apt/termux-main/pool/main/liba/libandroid-spawn/
```

Otherwise re-fetch the two sources from the recipe above. `stage-jre.py`
verifies alignment across the whole staged tree on every run, so a regression
fails staging rather than shipping.
