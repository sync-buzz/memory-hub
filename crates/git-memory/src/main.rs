mod cli;
mod doctor;
mod exit;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run(std::env::args_os()).into()
}
