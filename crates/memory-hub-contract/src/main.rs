use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use memory_hub_contract::{FakeServerTarget, ReleaseBinaryTarget, ServerTarget, run_contract};

#[derive(Debug, Parser)]
#[command(about = "Run the Memory Hub black-box behavioral contract")]
struct Cli {
    /// A shipped memory-hub executable (invoked as `memory-hub mcp`).
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "fake_binary",
        required_unless_present = "fake_binary"
    )]
    release_binary: Option<PathBuf>,

    /// The deterministic fake server executable included with this harness.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "release_binary",
        required_unless_present = "release_binary"
    )]
    fake_binary: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = Output::Human)]
    output: Output,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Output {
    #[default]
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum Code {
    Success = 0,
    ContractFailed = 1,
    Internal = 70,
}

impl From<Code> for ExitCode {
    fn from(code: Code) -> Self {
        Self::from(code as u8)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let target: Box<dyn ServerTarget> = if let Some(binary) = cli.release_binary {
        Box::new(ReleaseBinaryTarget::new(binary))
    } else if let Some(binary) = cli.fake_binary {
        Box::new(FakeServerTarget::new(binary))
    } else {
        unreachable!("clap requires exactly one target")
    };
    let report = run_contract(target.as_ref());
    let render = match cli.output {
        Output::Human => render_human(&report),
        Output::Json => render_json(&report),
    };
    if let Err(error) = render {
        eprintln!("memory-hub-contract: unable to render report: {error}");
        return Code::Internal.into();
    }
    if report.passed {
        Code::Success.into()
    } else {
        Code::ContractFailed.into()
    }
}

fn render_human(report: &memory_hub_contract::ContractReport) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "Memory Hub contract target: {}", report.target)?;
    for scenario in &report.scenarios {
        if scenario.passed {
            writeln!(output, "[ok] {}", scenario.name)?;
        } else {
            writeln!(
                output,
                "[error] {}: {}",
                scenario.name,
                scenario.failure.as_deref().unwrap_or("unknown failure")
            )?;
        }
    }
    writeln!(
        output,
        "Result: {}",
        if report.passed { "passed" } else { "failed" }
    )
}

fn render_json(report: &memory_hub_contract::ContractReport) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, report).map_err(io::Error::other)?;
    writeln!(output)
}

#[cfg(test)]
mod tests {
    use super::Code;

    #[test]
    fn contract_exit_codes_do_not_drift() {
        assert_eq!(Code::Success as u8, 0);
        assert_eq!(Code::ContractFailed as u8, 1);
        assert_eq!(Code::Internal as u8, 70);
    }
}
