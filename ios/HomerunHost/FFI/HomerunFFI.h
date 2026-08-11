/*
 * The C surface of `rust/homerun-pumpkin-ffi`, hand-written because the crate
 * generates no header. It must match `rust/homerun-pumpkin-ffi/src/lib.rs`
 * exactly — a signature that drifts links fine and corrupts the stack at
 * runtime. See docs/ffi.md.
 *
 * Every function that returns a string returns a heap-allocated JSON document
 * the caller owns. Free it with homerun_free_string, including the ones from
 * failed calls: those are the console's error lines, and leaking them leaks
 * the console over a long session.
 */
#ifndef HOMERUN_FFI_H
#define HOMERUN_FFI_H

#include <stdint.h>

/* Bumped whenever the shape of this surface changes. The host checks it at
 * launch: a mismatch means the staged .a is not the one this source expects. */
uint32_t homerun_abi_version(void);

void homerun_free_string(char *ptr);

/*
 * Call into `homerun-core`: the decisions this app shares with the desktop and
 * Android — which jar to run, what a tunnel config says, when a handshake has
 * failed for good, what a console line means, which config files a server
 * needs before it starts.
 *
 * `method` and `args` are NUL-terminated UTF-8; `args` is a JSON object.
 * Passing NULL for either yields an error envelope rather than a crash.
 *
 * This reaches the same dispatch Android reaches over JNI, so the two
 * platforms cannot disagree about what a method means — only about how a
 * string crosses the boundary. Method catalogue in docs/core-bridge.md.
 *
 * Prefer the typed wrappers in Core.swift to calling this directly; they free
 * the reply on every path, which is easy to miss on the error ones.
 */
char *homerun_core_call(const char *method, const char *args);

/*
 * Blocks for the server's entire lifetime. MUST run on a dedicated thread
 * with at least a 16 MB stack — the 512 KB default overflows inside the
 * engine and kills the app with no panic report.
 *
 * One JSON request rather than arguments, so a new setting does not change
 * this signature:
 *
 *   { "serverId": "…", "dataDir": "…", "port": 25565,
 *     "settings": { "env": {…}, "gameType": "native-crossplay",
 *                   "resolved": [{ "name": "Notch", "id": "069a79f4-…" }] } }
 *
 * `port` 0 means the default. `settings` is optional; omitting it starts the
 * server on the engine's own configuration, which is what a host that has not
 * been taught to send them does — and which is announced on the console,
 * because the engine's defaults are not the player's choices.
 */
char *homerun_server_start(const char *request_json);

/*
 * What that request's settings would apply, without starting anything.
 *
 * Pure. Exists because homerun_server_start's arguments are otherwise only
 * observable by starting a real server: a misspelled key compiles, links, and
 * yields a server on defaults with nothing saying so. ios/coretest calls this
 * with a request built by the same Swift the app uses.
 */
char *homerun_server_settings_preview(const char *request_json);

/*
 * Backups.
 *
 * Separate from homerun_core_call on purpose. That dispatch is shared with
 * Android and every method in it is instantaneous and pure, so it gets called
 * from the main thread without a second thought. These are not: two of them
 * open TLS connections and block for minutes.
 *
 * Declared unconditionally. A build without the engine still exports all five;
 * they answer "this copy cannot back up worlds".
 */

/* Whether this build links a backup engine. 0 on Android and host builds. */
uint32_t homerun_backup_available(void);

/* The newest snapshot, as homerun-core's Snapshot shape, or null. Networked —
 * seconds, not milliseconds. Never call this on the main thread. */
char *homerun_backup_latest_snapshot(const char *request_json);

/* Runs one backup or restore to completion. BLOCKS, for minutes, and MUST run
 * on a dedicated thread with at least an 8 MB stack — the same rule, for the
 * same reason, as homerun_server_start. One at a time; a second call while one
 * is in flight is an error, not a queue. */
char *homerun_backup_run(const char *request_json);

/* Progress since `cursor`. Cheap, and safe from the main thread while
 * homerun_backup_run blocks another one. Same idiom as
 * homerun_server_logs_since. `total` of 0 means "not known yet". */
char *homerun_backup_progress_since(uint64_t cursor);

/* Cooperative, and coarse: it lands at the next phase boundary and cannot
 * interrupt a transfer already under way. Never blocks, and is not an error
 * when nothing is running. */
char *homerun_backup_cancel(void);

char *homerun_server_stop(void);
char *homerun_server_state(void);
char *homerun_server_stats(void);
char *homerun_server_players(void);
char *homerun_server_logs_since(uint64_t cursor);
char *homerun_server_command(const char *command);

#endif /* HOMERUN_FFI_H */
