/*
 * The C surface of `homerun-pumpkin-ffi`, for Swift.
 *
 * Add this to the app target's bridging header (or expose it through a module
 * map) and the symbols below become callable from Swift directly.
 *
 * Contract, in full, in `docs/ffi.md`. The two rules that matter most:
 *
 *   1. Every function returning `char *` returns a heap-allocated JSON string
 *      that the caller MUST release with `homerun_free_string`. Leaking these
 *      leaks the server's entire console over a long session.
 *
 *   2. Fallible calls answer {"ok":true,...} or {"ok":false,"error":"..."}.
 *      Error strings are shown to players and are written for players; surface
 *      them rather than rewording them.
 *
 * Check `homerun_abi_version()` at startup. It is bumped whenever this surface
 * changes shape, and a mismatch means the app and the library disagree about
 * what these functions do.
 */

#ifndef HOMERUN_FFI_H
#define HOMERUN_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Bumped when this surface changes shape. */
uint32_t homerun_abi_version(void);

/* Release any string returned by this library. Passing anything else is UB. */
void homerun_free_string(char *ptr);

/*
 * Call into `homerun-core`: the shared decisions this app makes with the
 * desktop and Android — which jar to run, what a tunnel config says, when a
 * handshake has failed for good, what a console line means, which config files
 * to write.
 *
 * `method` and `args` are NUL-terminated UTF-8; `args` is a JSON object.
 * Passing NULL for either yields an error envelope rather than a crash.
 *
 * This is the same dispatch Android reaches over JNI, so the two platforms
 * cannot disagree about what a method means. See `docs/core-bridge.md` for the
 * method catalogue, and `Core.swift` for typed wrappers — prefer those to
 * calling this directly.
 */
char *homerun_core_call(const char *method, const char *args);

/*
 * The engine. One server per process: the engine keeps global state and
 * switches worlds by process CWD.
 *
 * `homerun_server_start` blocks until the server is accepting connections and
 * MUST be called on a dedicated thread with at least 16 MB of stack — world
 * generation recurses deeply enough to overflow the default.
 */
char *homerun_server_start(const char *server_id, const char *data_dir, uint16_t port);
char *homerun_server_stop(void);
char *homerun_server_state(void);
char *homerun_server_stats(void);
char *homerun_server_players(void);
char *homerun_server_logs_since(uint64_t cursor);
char *homerun_server_command(const char *command);

#ifdef __cplusplus
}
#endif

#endif /* HOMERUN_FFI_H */
