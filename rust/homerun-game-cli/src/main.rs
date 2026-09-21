mod cli;
mod prepare;
mod protocol;
mod runner;

fn main() {
    if let Err(message) = cli::run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
