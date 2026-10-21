mod cli;
mod client;
mod extensions;
mod import;
mod network;
mod prepare;
mod protocol;
mod runner;
mod runtime;

use homerun_supervisor::platform;

/// The program that interrupts a Windows server for the stop ladder: this
/// one, re-run with the hidden verb. A separate process because attaching to
/// a console is process-wide and the event reaches whoever sends it; see the
/// supervisor's `console_interrupt.rs`.
fn interrupt_helper(pid: u32) -> Result<std::process::Command, String> {
    let exe = std::env::current_exe()
        .map_err(|_| "this program could not find itself to interrupt the server.".to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .arg(platform::CONSOLE_BREAK_VERB)
        .arg(pid.to_string());
    Ok(command)
}

fn main() {
    // Before anything else, and before `cli::run` installs a Ctrl+C handler:
    // the helper is a process that exists only to send one event.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(parsed) = platform::console_break_args(&args) {
        std::process::exit(match parsed {
            Ok(pid) => platform::console_break(pid),
            Err(message) => {
                eprintln!("{message}");
                platform::console_break_exit::USAGE
            }
        });
    }
    platform::set_interrupt_helper(interrupt_helper);
    // A runner started from a terminal on a platform without DPAPI has no
    // host to give it a key; a developer can name a file holding one. Electron
    // never sets this: it sends the key in `hello`.
    if let Some(path) = std::env::var_os("HOMERUN_SEAL_KEY_FILE") {
        if let Err(message) =
            homerun_supervisor::local_secret::load_key_file(std::path::Path::new(&path))
        {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
    if let Err(message) = cli::run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
