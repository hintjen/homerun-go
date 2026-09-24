//! A stand-in for a host's `java`, for the runner's lifecycle tests.
//!
//! Not a Java. A test copies this program into a fixture as `bin/java(.exe)`
//! and writes two files beside `bin/`:
//!
//! - `major`: the major version `java -version` should report;
//! - `game`: the program to run in its place, with the same arguments.
//!
//! `-version` prints a banner the way Temurin does, on stderr. Anything else
//! writes `options` -- each JVM option variable it was given, one
//! `NAME=value` per line -- then runs `game` with the arguments, stdin, stdout and stderr it was given and
//! exits with its code -- what `java -jar server.jar ...` looks like from the
//! outside. It exists because the runner spawns a real executable (a batch
//! file cannot be put in a job object directly), and a libtest binary refuses
//! `-version` as an option.

use std::path::PathBuf;
use std::process::{exit, Command};

fn main() {
    let home: PathBuf = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(|bin| bin.parent()).map(PathBuf::from))
        .unwrap_or_default();
    let read = |name: &str| {
        std::fs::read_to_string(home.join(name))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("-version") {
        eprintln!("openjdk version \"{}.0.2\" 2026-01-20 LTS", read("major"));
        eprintln!("OpenJDK Runtime Environment (fake)");
        return;
    }
    let options: String = ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS"]
        .iter()
        .filter_map(|key| {
            std::env::var(key).ok().map(|value| {
                format!(
                    "{key}={value}
"
                )
            })
        })
        .collect();
    let _ = std::fs::write(home.join("options"), options);
    let status = Command::new(read("game"))
        .args(&args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("fake java could not run its game: {e}");
            exit(127)
        });
    exit(status.code().unwrap_or(1));
}
