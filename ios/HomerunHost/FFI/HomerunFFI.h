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

/* Blocks for the server's entire lifetime. MUST run on a dedicated thread
 * with at least a 16 MB stack — the 512 KB default overflows inside the
 * engine and kills the app with no panic report. port 0 means the default. */
char *homerun_server_start(const char *server_id, const char *data_dir, uint16_t port);

char *homerun_server_stop(void);
char *homerun_server_state(void);
char *homerun_server_stats(void);
char *homerun_server_players(void);
char *homerun_server_logs_since(uint64_t cursor);
char *homerun_server_command(const char *command);

#endif /* HOMERUN_FFI_H */
