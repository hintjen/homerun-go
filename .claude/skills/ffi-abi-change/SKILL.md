---
name: ffi-abi-change
description: Add, change, or remove a `homerun_*` C ABI export in homerun-pumpkin-ffi and wire it through to both hosts. Use when a host needs to call something new in Rust, when an export's shape changes, or when diagnosing UnsatisfiedLinkError, "the native core has no method", an ABI mismatch in logcat, a server backend that is silently unavailable while the rest of the app works, or a native change that seems not to have taken effect. Not for adding a `core.*` method — that is a dispatch arm, see docs/core-bridge.md.
---

# Changing the C ABI

One export is **eight touchpoints across three languages**, and most ways of
getting it wrong fail silently or point somewhere else entirely. This is the
order to do them in and what skipping each looks like.

## First: is this actually an ABI change?

Two different things live behind this crate, and they are wired up completely
differently:

| You want | Where it goes | Follow |
|---|---|---|
| A shared **decision** — a rule, a shape, a verdict | a match arm in `core_dispatch.rs` | [`docs/core-bridge.md` § Adding a method](../../../docs/core-bridge.md) |
| A new **capability of the running server** — something stateful the supervisor owns | a new `homerun_*` export | this skill |

If your thing is pure and instantaneous, it is almost certainly a
`core_dispatch` arm and **not** an ABI change. Those cost nothing to add and
need no version bump. Prefer them.

## The seven touchpoints

Work top to bottom; each one is cheap, and the whole set is what makes the
call actually reachable.

**1. The Rust export** — `rust/homerun-pumpkin-ffi/src/lib.rs`

```rust
#[no_mangle]
pub extern "C" fn homerun_server_thing() -> *mut c_char {
    guarded(|| json!({ "ok": true }).to_string())
}
```

Rules the surrounding code already follows:

- `guarded(...)` for every export — it catches panics, because unwinding across
  the boundary is undefined behaviour, not an error you can handle.
- Taking a string? `unsafe extern "C"`, and `borrow(ptr)` to read it — it
  returns `None` for null or invalid UTF-8, which you answer with `err(...)`.
- The caller frees the return with `homerun_free_string`. Every path returns an
  owned pointer, including errors.
- **Error strings are shown to players.** A test asserts they contain no
  `errno`, `unwrap`, `panicked at`, `Mutex`, or `null pointer`.

**2. The ABI version** — same file

Bump `FFI_ABI_VERSION` *and* add a line to the history in its doc comment
saying what changed and whether a host built against the previous one still
works. That comment is the only record of which versions are compatible.

Additive is about **behaviour, not signatures**. ABI 6 added two exports and
also moved *when the console gets cleared* out of `start`. It was still
additive — a host that calls neither behaves exactly as before — but only
because that fallback was deliberately kept. Say which it is.

**3. The JNI wrapper** — `rust/homerun-pumpkin-ffi/src/jni_bridge.rs`

```rust
#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeThing(
    env: JNIEnv, _class: JClass,
) -> jstring {
    let json = unsafe { take_json(crate::homerun_server_thing()) };
    to_jstring(&env, json)
}
```

The symbol name is `Java_` + the package path + the class + the method, and it
is matched **at runtime by string**. A typo compiles perfectly and throws
`UnsatisfiedLinkError` on first call.

**4. The Kotlin binding** — `android/.../NativeServer.kt`: an `external fun`
whose name matches the JNI symbol exactly.

**5. `EXPECTED_ABI`** — same file. Skipping this is the worst failure mode
here: on mismatch `NativeServer.available` stays false, so **the app runs
normally with no server backend at all**. It looks like a UI bug.

**6. The C header** — `ios/HomerunHost/FFI/HomerunFFI.h`. Missing declaration
means iOS fails to compile or link, which at least is loud.

**7. The Swift wrapper** — `ios/HomerunHost/FFI/HomerunFFI.swift`, in the
existing shape: `decode(homerun_…())`, or `x.withCString { decode(…($0)) }`
for arguments. iOS links statically and only *reports* the ABI at startup
(`AppDelegate`), so there is no runtime check to catch a mismatch there.

**8. The iOS harness's expectation** — `ios/coretest/main.swift`, the
`let expected: UInt32 = N` line. It is not shipping code, but
`scripts/check-abi.js` reads it, so **`npm test` fails until it is bumped** —
and it fails on a message that reads "is the staged .a stale?", which sends you
looking at the build rather than at this line. It sat at 3 while the crate
reached 7, which is why the check exists.

## When the export is for one platform only

Touchpoints 3 and 4 — the JNI wrapper and the Kotlin `external fun` — are for
things Kotlin calls. An export only iOS uses needs neither, and adding them
anyway means two symbols nothing invokes. Say so in the doc comment, so the
next reader does not go looking for the missing half.

**Touchpoint 5 is not skippable on that reasoning.** `EXPECTED_ABI` is a
comparison against the crate, not against what Android calls, so an iOS-only
export that bumps the version still disables Android's entire server backend
until the constant follows. That is the worst failure mode here and it looks
like a UI bug.

## Registering a callback rather than adding a call

Some things have to go the other way: the crate needs something only the host
can do (read the unified log, write to `os_log`). That is still an ABI change,
and the shape is a registration export.

```rust
pub type Sink = unsafe extern "C" fn(level: u8, message: *const c_char);

#[no_mangle]
pub extern "C" fn homerun_set_log_sink(sink: Option<Sink>) -> *mut c_char {
    guarded(move || { host_log::set(sink); json!({ "ok": true }).to_string() })
}
```

- **`Option<extern "C" fn …>` is FFI-safe and null-checks itself.** A host
  passing NULL arrives as `None`, which is how unregistering works — no
  pointer to dereference and no separate "clear" export.
- **The header needs a `typedef`**, and the Swift side takes it as
  `@convention(c)`. That means the Swift function **captures nothing**: a
  global `func` or a `let` closure with an empty capture list, never a method
  on an instance.
- **Copy the pointer out of its lock before calling it.** Holding the mutex
  across a call into the host deadlocks the moment that host logs anything
  itself, and it presents as a hang in whatever produced the line.
- **Neither side may unwind.** A Swift or Kotlin exception crossing into Rust
  is undefined behaviour exactly as a Rust panic crossing out is; `guarded`
  only covers the second.
- **Register it at launch, not at first use.** A callback that answers
  diagnostics is worth nothing if it is registered after the failure it would
  have explained.

## Then rebuild — properly

```bash
npm run android:build     # stages jniLibs, then gradle
npm test                  # core + FFI + the ABI check
```

`scripts/check-abi.js` compares the crate's `FFI_ABI_VERSION` against each
host's expectation — Android's `NativeServer.EXPECTED_ABI` and the iOS
harness's `let expected` in `ios/coretest/main.swift`. The shipping iOS app
still has no expectation of its own: it links statically and only reports the
version at startup.

**Gradle alone is not enough.** The npm script restages the `.so` into
`jniLibs` first. Skip it and a fresh APK runs against yesterday's library — a
*runtime* "no such method", not a build failure.

To prove what actually shipped:

```bash
"$ANDROID_HOME"/ndk/*/toolchains/llvm/prebuilt/*/bin/llvm-nm \
  --dynamic --defined-only android/app/src/main/jniLibs/x86_64/libhomerun_pumpkin_ffi.so \
  | grep -i thing
```

Incremental builds usually rebuild only the ABI you are running, so the other
architecture's `.so` can stay stale for days and pass every emulator test.

Then verify on a device — see the `android-emulator` skill.

## Triage

**App runs but hosts nothing; no server UI works.** ABI mismatch — check
logcat for `HomerunNative`. You bumped the crate and not `EXPECTED_ABI`, or
the `.so` is stale.

**`UnsatisfiedLinkError` on one method.** The JNI symbol name does not match
the Kotlin `external fun`. Compare them character by character.

**The call returns but nothing happens.** Look for a `runCatching { }` with no
`onFailure` swallowing it. Add the failure log *permanently* — a silent no-op
here produces an impossible-looking situation that costs far more than the log
line ever will.

**iOS link error naming your symbol.** Missing from `HomerunFFI.h`.

**Works on the emulator, fails on a phone.** The arm64 `.so` was never rebuilt.

---

## Found something this skill got wrong?

Fix it here, in the same commit as the work that revealed it — while you still
remember what was actually confusing. A trap you fell into, a command that did
not behave as described, a step that was missing, an instruction that read two
ways: all of it belongs in this file. The test is whether the next session
avoids the mistake you just made.

If the gap is big enough to be its own skill, say so and offer to write it —
do not create one unasked.
