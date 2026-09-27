//! The installer and maintenance runtime.
//!
//! This crate is the native runtime a generated installer embeds and an
//! installation persists. It is a composition root, not an engine: it owns the
//! lifecycle command surface, the presentation frontends, the maintenance
//! handoffs between processes, and the wiring that turns an embedded or verified
//! package into a transaction. The engines it drives — planning, transaction,
//! execution, protocol, acquisition, update — live in their own crates and know
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

mod acquire;
mod bootstrap;
mod cli;
#[cfg(feature = "console")]
mod console;
mod context;
pub mod entry;
mod execute;
mod frontend;
#[cfg(feature = "gui")]
mod gui;
mod handoff;
mod lifecycle;
mod package;
mod recovery;
mod state;
mod uninstall;
mod update;
mod worker;

use zup_presentation::ProcessOutcome;

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

/// The runtime's own parser, for the product-surface tests and for documentation
/// generation.
pub fn command() -> clap::Command {
    cli::parser()
}

/// Write a failure in the format the caller asked for.
///
/// Human output goes to stderr through the binary's own error path; the machine
/// formats go to stdout, because stdout is the channel an automation system is
/// reading and a failure it cannot parse is a failure it will misreport.
fn emit_failure(output: zup_presentation::OutputFormat, error: &miette::Report) {
    use zup_presentation::{AutomationEvent, AutomationResult, DiagnosticPresentation};

    let outcome = ProcessOutcome::from_message(&error.to_string());
    match output {
        zup_presentation::OutputFormat::Human => {}
        zup_presentation::OutputFormat::Json => {
            let mut result = AutomationResult::new(outcome, "", "");
            result.message = Some(error.to_string());
            if let Ok(value) = result.to_json() {
                println!("{value}");
            }
        }
        zup_presentation::OutputFormat::Jsonl => {
            let started = AutomationEvent::started("", "", "unknown");
            if let Ok(value) = serde_json::to_string(&started) {
                println!("{value}");
            }
            let event = AutomationEvent::Failed {
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

#[cfg(test)]
mod tests;
