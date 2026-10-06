//! The installer and maintenance runtime.
//!
//! This crate is the native runtime a generated installer embeds and an
//! installation persists. It is a composition root, not an engine: it owns the
//! lifecycle command surface, the presentation frontends, the maintenance
//! handoffs between processes, and the wiring that turns an embedded or verified
//! package into a transaction. The engines it drives - planning, transaction,
//! execution, protocol, acquisition, update - live in their own crates and know
//! nothing about a command line.
//!
//! What it deliberately does not know: how a `zup.toml` is written, how a source
//! tree becomes a plan, how a manifest is formatted or schema'd, how a release
//! is published. Those are the developer CLI's job, and the boundary is a package
//! boundary so neither side can grow into the other by accident.
//!
//! The frontend a process was built for is a compile-time fact of the binary that
//! started it and is passed in explicitly. There is no global to set and no
//! process-wide override to read back: a command reaches the frontend it was
//! given, or it does not compile.

#[cfg(windows)]
mod acquire;
#[cfg(windows)]
mod bootstrap;
#[cfg(windows)]
mod cli;
#[cfg(all(windows, feature = "console"))]
mod console;
#[cfg(windows)]
mod context;
pub mod entry;
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
mod package;
#[cfg(windows)]
mod recovery;
#[cfg(windows)]
mod state;
#[cfg(windows)]
mod uninstall;
#[cfg(windows)]
mod update;
#[cfg(windows)]
mod worker;

use zup_presentation::ProcessOutcome;

#[cfg(windows)]
use crate::context::RuntimeContext;

/// The process exit code a failure reports.
///
/// The same stable outcome vocabulary the machine-readable formats use, so a
/// script can branch on the exit code alone.
pub fn process_exit_code(error: &miette::Report) -> u8 {
    ProcessOutcome::from_message(&error.to_string())
        .code()
        .clamp(1, 255) as u8
}

/// Run the runtime as the frontend its binary was built for.
///
/// The entry point, the error report, the machine-readable failure, and the exit
/// code are decided here, once, so the three presentation binaries cannot drift
/// apart. They are three lines long and call this.
///
/// Native selection lives here and in `entry`, nowhere else: Windows runs the
/// worker-based lifecycle, Linux runs the in-process one. No other module
/// chooses a backend.
#[cfg(windows)]
pub fn run(frontend: zup_core::Frontend) -> miette::Result<()> {
    let cli = cli::parse();
    let output = cli.output_format();
    let context = RuntimeContext::new(frontend).with_output(output);
    let result = cli::dispatch(cli, context);
    if let Err(error) = &result
        // A machine-readable consumer that has already been told the run failed
        // is not told twice. That used to be a process-wide flag; it is now a
        // property of the error, so it cannot be set by an unrelated command and
        // cannot be left set by a failed one.
        && !execute::already_reported(error)
    {
        emit_failure(output, error);
    }
    result
}

/// Run the runtime as the frontend its binary was built for, on Linux.
///
/// The same three-binaries-one-entry shape as Windows, with the in-process
/// Linux lifecycle behind it. No worker, no escalation: user scope means this
/// process owns every directory it touches.
#[cfg(target_os = "linux")]
pub fn run(frontend: zup_core::Frontend) -> miette::Result<()> {
    crate::linux::run(frontend)
}

/// The runtime's own parser, for the product-surface tests and for documentation
/// generation.
#[cfg(windows)]
pub fn command() -> clap::Command {
    cli::parser()
}

/// Write a failure in the format the caller asked for.
///
/// Human output goes to stderr through the binary's own error path; the machine
/// formats go to stdout, because stdout is the channel an automation system is
/// reading and a failure it cannot parse is a failure it will misreport.
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
