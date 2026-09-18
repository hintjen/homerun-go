//! Write the descriptor's JSON Schema.
//!
//! The monorepo pins the output at `games/schema/game.v0.json` and checks the
//! two agree, so editors and CI can check a `game.json` without a Rust
//! toolchain. The Rust types stay the source of truth —
//! `engine::schema::tests` fails if a field exists in them and not in here.
//!
//! ```bash
//! npm run schema:descriptor                 # to stdout
//! npm run schema:descriptor -- games/schema/game.v0.json
//! ```
//!
//! An example rather than a `[[bin]]` on purpose: a bin would be built by
//! every `cargo build` of this crate, including the iOS and Android
//! cross-compiles, for a developer tool neither of them has any use for.

use std::io::Write;

fn main() {
    let document = homerun_core::engine::schema::document();

    match std::env::args().nth(1) {
        Some(path) => {
            if let Err(err) = std::fs::write(&path, &document) {
                eprintln!("could not write {path}: {err}");
                std::process::exit(1);
            }
            eprintln!("wrote {path}");
        }
        None => {
            // `print!` would panic on a closed pipe -- `… | head` is the
            // obvious way to read this.
            let _ = std::io::stdout().write_all(document.as_bytes());
        }
    }
}
