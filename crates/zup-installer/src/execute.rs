use crate::lifecycle::PreparedRuntime;
use zup_exec::LifecycleAction;
use zup_presentation::{InstallerEvent, InstallerResult, OutputFormat, ProcessOutcome};
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, OperationControl, RuntimeEvent, RuntimeRequest,
};

/// process-level failure writer must not then add a second, differently shaped
#[derive(Debug)]
pub struct ExecutionError {
    error: miette::Report,
    reported: bool,
}

impl ExecutionError {
    pub fn reported(error: miette::Report) -> Self {
        Self {
            error,
            reported: true,
        }
    }

    pub fn silent(error: miette::Report) -> Self {
        Self {
            error,
            reported: false,
        }
    }

    pub fn was_reported(&self) -> bool {
        self.reported
    }
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl miette::Diagnostic for ExecutionError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        self.error.code()
    }

    fn severity(&self) -> Option<miette::Severity> {
        self.error.severity()
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        self.error.help()
    }

    fn url<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        self.error.url()
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.error.source_code()
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        self.error.labels()
    }

    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn miette::Diagnostic> + 'a>> {
        self.error.related()
    }

    fn diagnostic_source(&self) -> Option<&dyn miette::Diagnostic> {
        Some(self.error.as_ref() as &dyn miette::Diagnostic)
    }
}

impl From<miette::Report> for ExecutionError {
    fn from(error: miette::Report) -> Self {
        Self::silent(error)
    }
}

pub fn already_reported(error: &miette::Report) -> bool {
    error
        .downcast_ref::<ExecutionError>()
        .is_some_and(ExecutionError::was_reported)
}

pub fn execute(prepared: PreparedRuntime) -> miette::Result<()> {
    execute_with_policy(prepared, ExecutionPolicy::NonInteractive).map_err(Into::into)
}

pub fn execute_with_policy(
    prepared: PreparedRuntime,
    policy: ExecutionPolicy,
) -> Result<(), ExecutionError> {
    let drifted = drifted_keys(&prepared.request);
    let control = OperationControl::new(policy);
    match execute_with_control_signal(prepared, &control)? {
        InstallOutcome::Committed => {
            println!("committed");
            for key in drifted {
                eprintln!("left drifted resource untouched: {key}");
            }
            Ok(())
        }
        other => Err(ExecutionError::silent(miette::miette!(
            "transaction: {other:?}"
        ))),
    }
}

pub fn execute_with_control_signal(
    prepared: PreparedRuntime,
    control: &OperationControl,
) -> miette::Result<InstallOutcome> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            prepared.backend.cleanup_overlay(&prepared.request);
            miette::miette!("runtime: {error}")
        })?;
    let cancel = control.cancellation();
    runtime
        .block_on(async {
            let PreparedRuntime {
                request, backend, ..
            } = prepared;
            let operation = zup_runtime::execute(&*backend, request, control);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| zup_runtime::SessionError::Protocol(error.to_string()))?;
                    cancel.cancel();
                    let _ = control.events().send(RuntimeEvent::StateChanged {
                        state: zup_runtime::RuntimeState::Cancelled,
                    });
                    operation.await
                }
            }
        })
        .map_err(|error| miette::miette!("install session: {error}"))
}

fn drifted_keys(request: &RuntimeRequest) -> Vec<String> {
    request
        .transaction_plan
        .retired_keys
        .iter()
        .map(|key| format!("{key:?}"))
        .collect()
}

pub fn execute_frontend(
    prepared: PreparedRuntime,
    output: OutputFormat,
    action: LifecycleAction,
) -> Result<(), ExecutionError> {
    let request = prepared.request.clone();
    let application = request.app_id.to_string();
    let version = request.app_version.to_string();
    let scope = Some(request.scope);
    let install_directory = request
        .transaction_plan
        .install_directory
        .as_ref()
        .map(ToString::to_string);
    let drifted = drifted_keys(&request);
    let control = OperationControl::new(ExecutionPolicy::NonInteractive);
    let mut receiver = control.subscribe();
    if output == OutputFormat::Jsonl {
        let started = InstallerEvent::started(
            &application,
            &version,
            crate::maintenance::action_name(action),
        );
        println!(
            "{}",
            serde_json::to_string(&started).map_err(|error| miette::miette!("output: {error}"))?
        );
    }
    let log_path = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let pump_log_path = log_path.clone();
    let jsonl = output == OutputFormat::Jsonl;
    let pump = std::thread::spawn(move || {
        loop {
            let event = match receiver.blocking_recv() {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            };
            if let RuntimeEvent::LogPath { path } = &event {
                *pump_log_path.lock().expect("log path state") = Some(path.clone());
            }
            for output_event in automation_events(&event) {
                if jsonl && let Ok(line) = serde_json::to_string(&output_event) {
                    println!("{line}");
                }
            }
            if is_terminal(&event) {
                return true;
            }
        }
    });
    let outcome = execute_with_control_signal(prepared, &control);
    let terminal_seen = pump.join().unwrap_or(false);
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            return Err(ExecutionError::reported(report_failure(
                output,
                &application,
                &version,
                scope,
                install_directory.as_deref(),
                log_path.lock().expect("log path state").as_deref(),
                &drifted,
                terminal_seen,
                error,
            )));
        }
    };
    let process = process_outcome(&outcome);
    match output {
        OutputFormat::Human => {
            if outcome == InstallOutcome::Committed {
                println!("committed");
                for key in drifted {
                    eprintln!("left drifted resource untouched: {key}");
                }
                Ok(())
            } else {
                Err(ExecutionError::silent(miette::miette!(
                    "transaction: {outcome:?}"
                )))
            }
        }
        OutputFormat::Json => {
            let mut result = InstallerResult::new(process, application, version);
            result.scope = scope;
            result.install_directory = install_directory;
            result.log_path = log_path.lock().expect("log path state").clone();
            result.drift = drifted;
            if process != ProcessOutcome::Success {
                result.message = Some(format!("{outcome:?}"));
            }
            println!(
                "{}",
                result
                    .to_json()
                    .map_err(|error| ExecutionError::silent(miette::miette!("output: {error}")))?
            );
            if process == ProcessOutcome::Success {
                Ok(())
            } else {
                Err(ExecutionError::silent(miette::miette!(
                    "transaction: {outcome:?}"
                )))
            }
        }
        OutputFormat::Jsonl => {
            if !terminal_seen {
                let event = InstallerEvent::Completed { outcome: process };
                println!(
                    "{}",
                    serde_json::to_string(&event).map_err(|error| ExecutionError::silent(
                        miette::miette!("output: {error}")
                    ))?
                );
            }
            if !matches!(outcome, InstallOutcome::Committed) {
                return Err(ExecutionError::silent(miette::miette!(
                    "transaction: {outcome:?}"
                )));
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report_failure(
    output: OutputFormat,
    application: &str,
    version: &str,
    scope: Option<zup_core::SelectedScope>,
    install_directory: Option<&str>,
    log_path: Option<&str>,
    drift: &[String],
    terminal_seen: bool,
    error: miette::Report,
) -> miette::Report {
    let process = ProcessOutcome::from_message(&error.to_string());
    let message = error.to_string();
    match output {
        OutputFormat::Human => {}
        OutputFormat::Json => {
            let mut result = InstallerResult::new(process, application, version);
            result.scope = scope;
            result.install_directory = install_directory.map(ToOwned::to_owned);
            result.log_path = log_path.map(ToOwned::to_owned);
            result.drift = drift.to_vec();
            result.message = Some(message);
            if let Ok(value) = result.to_json() {
                println!("{value}");
            }
        }
        OutputFormat::Jsonl => {
            if !terminal_seen {
                let event = InstallerEvent::Failed {
                    outcome: process,
                    code: process.code(),
                    message: message.clone(),
                    diagnostic: Some(zup_presentation::DiagnosticPresentation::from_message(
                        &message,
                        process == ProcessOutcome::RecoveryRequired,
                    )),
                };
                if let Ok(value) = serde_json::to_string(&event) {
                    println!("{value}");
                }
            }
        }
    }
    error
}

pub fn is_terminal(event: &RuntimeEvent) -> bool {
    zup_session::is_terminal(event)
}

pub fn process_outcome(outcome: &InstallOutcome) -> ProcessOutcome {
    zup_session::process_outcome(outcome)
}

pub fn automation_events(event: &RuntimeEvent) -> Vec<InstallerEvent> {
    zup_session::automation_events(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_execution_error_says_whether_the_consumer_was_told() {
        let reported: miette::Report =
            ExecutionError::reported(miette::miette!("the install failed")).into();
        assert!(already_reported(&reported));
        // The same conversion must not swallow the run's own message: the report is
        assert!(
            reported.to_string().contains("the install failed"),
            "{reported}"
        );

        let silent: miette::Report = ExecutionError::silent(miette::miette!("boom")).into();
        assert!(!already_reported(&silent));
        let plain: miette::Report = ExecutionError::from(miette::miette!("boom")).into();
        assert!(!already_reported(&plain));
    }

    #[test]
    fn a_log_path_is_not_a_phase() {
        assert!(
            automation_events(&RuntimeEvent::LogPath {
                path: "C:/tmp/zup.log".into()
            })
            .is_empty()
        );
        assert_eq!(
            automation_events(&RuntimeEvent::PreflightStarted).len(),
            1,
            "a phase a consumer can wait on is a phase"
        );
    }
}
