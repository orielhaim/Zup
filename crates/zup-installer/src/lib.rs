#[cfg(windows)]
mod acquire;
#[cfg(windows)]
mod bootstrap;
#[cfg(windows)]
mod cli;
#[cfg(all(windows, feature = "console"))]
mod console;
#[cfg(windows)]
mod execute;
#[cfg(windows)]
mod frontend;
#[cfg(windows)]
mod handoff;
#[cfg(all(windows, feature = "gui"))]
pub mod host;
#[cfg(windows)]
mod lifecycle;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod maintenance;
#[cfg(windows)]
mod package;
#[cfg(any(windows, target_os = "linux"))]
pub mod run;

use zup_presentation::ProcessOutcome;

#[cfg(windows)]
use crate::run::RuntimeContext;

pub fn process_exit_code(error: &miette::Report) -> u8 {
    ProcessOutcome::from_message(&error.to_string())
        .code()
        .clamp(1, 255) as u8
}

#[cfg(windows)]
pub fn run(frontend: zup_core::Frontend) -> miette::Result<()> {
    let cli = cli::parse();
    let output = cli.output_format();
    let context = RuntimeContext::new(frontend).with_output(output);
    let result = cli::dispatch(cli, context);
    if let Err(error) = &result
        && !execute::already_reported(error)
    {
        emit_failure(output, error);
    }
    result
}

#[cfg(target_os = "linux")]
pub fn run(frontend: zup_core::Frontend) -> miette::Result<()> {
    crate::linux::run(frontend)
}

#[cfg(windows)]
pub fn command() -> clap::Command {
    cli::parser()
}

#[cfg(windows)]
fn emit_failure(output: zup_presentation::OutputFormat, error: &miette::Report) {
    use zup_presentation::{DiagnosticPresentation, InstallerEvent, InstallerResult};

    let outcome = ProcessOutcome::from_message(&error.to_string());
    match output {
        zup_presentation::OutputFormat::Human => {}
        zup_presentation::OutputFormat::Json => {
            let mut result = InstallerResult::new(outcome, "", "");
            result.message = Some(error.to_string());
            if let Ok(value) = result.to_json() {
                println!("{value}");
            }
        }
        zup_presentation::OutputFormat::Jsonl => {
            let started = InstallerEvent::started("", "", "unknown");
            if let Ok(value) = serde_json::to_string(&started) {
                println!("{value}");
            }
            let event = InstallerEvent::Failed {
                outcome,
                code: outcome.code(),
                message: error.to_string(),
                diagnostic: Some(DiagnosticPresentation::from_message(
                    &error.to_string(),
                    outcome == ProcessOutcome::RecoveryRequired,
                )),
            };
            if let Ok(value) = serde_json::to_string(&event) {
                println!("{value}");
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests;
