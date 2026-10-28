#[cfg(windows)]
mod acquire;
#[cfg(windows)]
mod cli;
#[cfg(feature = "console")]
mod console;
#[cfg(windows)]
mod execute;
#[cfg(windows)]
mod frontend;
#[cfg(windows)]
mod handoff;
#[cfg(windows)]
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
#[cfg(any(windows, target_os = "linux"))]
mod terminal;

use zup_presentation::ProcessOutcome;

#[cfg(windows)]
use crate::run::RuntimeContext;

pub fn process_exit_code(error: &miette::Report) -> i32 {
    error_outcome(error).code()
}

pub(crate) fn error_outcome(error: &miette::Report) -> ProcessOutcome {
    if let Some(failure) = error.downcast_ref::<terminal::ReportedFailure>() {
        return failure.0.outcome;
    }
    if let Some(failure) = error.downcast_ref::<terminal::Failure>() {
        return failure.outcome;
    }
    #[cfg(windows)]
    if let Some(error) = error.downcast_ref::<execute::ExecutionError>() {
        return error.outcome();
    }
    ProcessOutcome::from_message(&error.to_string())
}

pub fn run(frontend: zup_core::Frontend) -> miette::Result<()> {
    #[cfg(windows)]
    {
        if frontend != zup_core::Frontend::Gui && !native_command() {
            return host::terminal(frontend);
        }
        let cli = cli::parse();
        let output = cli.output_format();
        let context = RuntimeContext::new(frontend).with_output(output);
        let result = cli::dispatch(cli, context);
        if let Err(error) = &result
            && !execute::already_reported(error)
            && error.downcast_ref::<terminal::ReportedFailure>().is_none()
        {
            emit_failure(output, error);
        }
        result
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::run(frontend)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = frontend;
        Err(miette::miette!(
            "no installer backend for this host: installers run on Windows or Linux"
        ))
    }
}

pub fn command() -> clap::Command {
    #[cfg(windows)]
    {
        host::command()
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::command()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        clap::Command::new("setup")
    }
}

#[cfg(windows)]
fn emit_failure(output: zup_presentation::OutputFormat, error: &miette::Report) {
    use zup_presentation::{DiagnosticPresentation, InstallerEvent, InstallerResult};

    let outcome = error_outcome(error);
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
                diagnostic: Some(DiagnosticPresentation::for_outcome(
                    outcome,
                    &error.to_string(),
                )),
            };
            if let Ok(value) = serde_json::to_string(&event) {
                println!("{value}");
            }
        }
    }
}

#[cfg(windows)]
fn native_command() -> bool {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.iter().any(|argument| argument == "--source")
        && host::command()
            .try_get_matches_from(std::env::args_os())
            .is_ok()
    {
        return false;
    }
    arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "__worker" | "__recover" | "__uninstall_runner" | "__terminal_runner" | "__upgrade"
        ) || matches!(
            argument.as_str(),
            "--handoff" | "--acquired" | "--source" | "--ui"
        )
    })
}
