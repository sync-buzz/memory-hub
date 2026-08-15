mod cli;
mod config;
mod doctor;
mod exit;
mod model;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run(std::env::args_os()).into()
}
