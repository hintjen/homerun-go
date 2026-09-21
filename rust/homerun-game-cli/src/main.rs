mod cli;
mod prepare;
mod protocol;
mod runner;
mod runtime;

fn main() {
    if let Err(message) = cli::run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
