---
name: ffi-abi-change
description: Add, change, or remove a `homerun_*` C ABI export in homerun-pumpkin-ffi and wire it through to both hosts. Use when a host needs to call something new in Rust, when an export's shape changes, or when diagnosing UnsatisfiedLinkError, "the native core has no method", an ABI mismatch in logcat, a server backend that is silently unavailable while the rest of the app works, or a native change that seems not to have taken effect. Not for adding a `core.*` method — that is a dispatch arm, see docs/core-bridge.md.
---

# Changing the C ABI

One export is **seven touchpoints across three languages**, and most ways of
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
(`AppDelegate`), so there is no `EXPECTED_ABI` to update — and no runtime
check to catch a mismatch either.

## Then rebuild — properly

```bash
npm run android:build     # stages jniLibs, then gradle
npm test                  # core + FFI + the ABI check
```

`scripts/check-abi.js` compares the crate's `FFI_ABI_VERSION` against each
host's expectation. It only knows about Android today; iOS is not checked.

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
