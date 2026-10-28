use std::cell::RefCell;
use zup_session::{automation_events, process_outcome};

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
    outcome: ProcessOutcome,
}

impl ExecutionError {
    pub fn reported(error: miette::Report) -> Self {
        let outcome = crate::error_outcome(&error);
        Self {
            error,
            reported: true,
            outcome,
        }
    }

    pub fn silent(error: miette::Report) -> Self {
        let outcome = crate::error_outcome(&error);
        Self {
            error,
            reported: false,
            outcome,
        }
    }

    pub fn outcome(&self) -> ProcessOutcome {
        self.outcome
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
    execute_observing(prepared, control, |_| {})
}

pub fn execute_observing(
    prepared: PreparedRuntime,
    control: &OperationControl,
    on_event: impl FnMut(&RuntimeEvent),
) -> miette::Result<InstallOutcome> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            prepared.backend.cleanup_overlay(&prepared.request);
            miette::miette!("runtime: {error}")
        })?;
    let PreparedRuntime {
        request, backend, ..
    } = prepared;
    runtime
        .block_on(observe_execution(
            zup_runtime::execute(&*backend, request, control),
            control,
            on_event,
        ))
        .map_err(|error| miette::miette!("install session: {error}"))
}

async fn observe_execution<T>(
    operation: impl std::future::Future<Output = Result<T, zup_runtime::SessionError>>,
    control: &OperationControl,
    mut on_event: impl FnMut(&RuntimeEvent),
) -> Result<T, zup_runtime::SessionError> {
    let mut receiver = control.subscribe();
    let cancel = control.cancellation();
    let mut interrupted = false;
    tokio::pin!(operation);
    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut operation => break result,
            signal = tokio::signal::ctrl_c(), if !interrupted => {
                signal.map_err(|error| zup_runtime::SessionError::Protocol(error.to_string()))?;
                interrupted = true;
                cancel.cancel();
                let _ = control.events().send(RuntimeEvent::StateChanged {
                    state: zup_runtime::RuntimeState::Cancelled,
                });
            }
            event = receiver.recv() => {
                match event {
                    Ok(event) => on_event(&event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break operation.await,
                }
            }
        }
    };
    loop {
        match receiver.try_recv() {
            Ok(event) => on_event(&event),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }
    outcome
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
    if output == OutputFormat::Jsonl {
        let started = InstallerEvent::started(
            &application,
            &version,
            crate::maintenance::action_name(action),
        );
        emit_line(
            &serde_json::to_string(&started).map_err(|error| miette::miette!("output: {error}"))?,
        );
    }
    let log_path = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let observed_log = log_path.clone();
    let jsonl = output == OutputFormat::Jsonl;
    let outcome = execute_observing(prepared, &control, |event| {
        note_progress(jsonl, &observed_log, event);
    });
    publish_outcome(
        output,
        &application,
        &version,
        scope,
        install_directory,
        log_path.lock().expect("log path state").clone(),
        &drifted,
        outcome,
    )
}

#[allow(clippy::too_many_arguments)]
fn publish_outcome(
    output: OutputFormat,
    application: &str,
    version: &str,
    scope: Option<zup_core::SelectedScope>,
    install_directory: Option<String>,
    log_path: Option<String>,
    drifted: &[String],
    outcome: miette::Result<InstallOutcome>,
) -> Result<(), ExecutionError> {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            return Err(ExecutionError::reported(report_failure(
                output,
                application,
                version,
                scope,
                install_directory.as_deref(),
                log_path.as_deref(),
                drifted,
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
                Err(ExecutionError::silent(outcome_error(&outcome)))
            }
        }
        OutputFormat::Json => {
            let mut result = InstallerResult::new(process, application, version);
            result.scope = scope;
            result.install_directory = install_directory;
            result.log_path = log_path;
            result.drift = drifted.to_vec();
            if process != ProcessOutcome::Success {
                result.message = Some(format!("{outcome:?}"));
            }
            emit_line(
                &result
                    .to_json()
                    .map_err(|error| ExecutionError::silent(miette::miette!("output: {error}")))?,
            );
            if process == ProcessOutcome::Success {
                Ok(())
            } else {
                Err(ExecutionError::reported(outcome_error(&outcome)))
            }
        }
        OutputFormat::Jsonl => {
            let event = if process == ProcessOutcome::Success {
                InstallerEvent::Completed { outcome: process }
            } else {
                let message = format!("{outcome:?}");
                InstallerEvent::Failed {
                    outcome: process,
                    code: process.code(),
                    diagnostic: Some(zup_presentation::DiagnosticPresentation::for_outcome(
                        process, &message,
                    )),
                    message,
                }
            };
            emit_line(
                &serde_json::to_string(&event)
                    .map_err(|error| ExecutionError::silent(miette::miette!("output: {error}")))?,
            );
            if !matches!(outcome, InstallOutcome::Committed) {
                return Err(ExecutionError::reported(outcome_error(&outcome)));
            }
            Ok(())
        }
    }
}

fn outcome_error(outcome: &InstallOutcome) -> miette::Report {
    crate::terminal::Failure {
        outcome: process_outcome(outcome),
        message: format!("transaction: {outcome:?}"),
    }
    .into()
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
    error: miette::Report,
) -> miette::Report {
    let process = crate::error_outcome(&error);
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
                emit_line(&value);
            }
        }
        OutputFormat::Jsonl => {
            let event = InstallerEvent::Failed {
                outcome: process,
                code: process.code(),
                message: message.clone(),
                diagnostic: Some(zup_presentation::DiagnosticPresentation::for_outcome(
                    process, &message,
                )),
            };
            if let Ok(value) = serde_json::to_string(&event) {
                emit_line(&value);
            }
        }
    }
    error
}

fn note_progress(jsonl: bool, log_path: &std::sync::Mutex<Option<String>>, event: &RuntimeEvent) {
    if let RuntimeEvent::LogPath { path } = event {
        *log_path.lock().expect("log path state") = Some(path.clone());
    }
    if !jsonl {
        return;
    }
    for output_event in automation_events(event) {
        if is_result_event(&output_event) {
            continue;
        }
        if let Ok(line) = serde_json::to_string(&output_event) {
            emit_line(&line);
        }
    }
}

fn is_result_event(event: &InstallerEvent) -> bool {
    matches!(
        event,
        InstallerEvent::Completed { .. } | InstallerEvent::Failed { .. }
    )
}

fn emit_line(line: &str) {
    let mut captured = false;
    OUTPUT.with(|output| {
        if let Some(lines) = output.borrow_mut().as_mut() {
            lines.push(line.to_owned());
            captured = true;
        }
    });
    if !captured {
        println!("{line}");
    }
}

thread_local! {
    static OUTPUT: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
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

    #[derive(Clone, Copy, Debug)]
    enum Sample {
        Committed,
        Failed,
        Cancelled,
        Quiet,
        Lagged,
        Reconciliation,
    }

    #[test]
    fn the_frontend_returns_from_the_operation_result() {
        for sample in [
            Sample::Committed,
            Sample::Failed,
            Sample::Cancelled,
            Sample::Quiet,
            Sample::Lagged,
            Sample::Reconciliation,
        ] {
            for output in [OutputFormat::Json, OutputFormat::Jsonl] {
                let (sender, receiver) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(drive_sample(sample, output));
                });
                let lines = receiver
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the frontend returns without a terminal event");
                let results = lines
                    .iter()
                    .filter(|line| {
                        line.contains("\"type\":\"completed\"")
                            || line.contains("\"type\":\"failed\"")
                    })
                    .count();
                if output == OutputFormat::Jsonl {
                    assert_eq!(results, 1, "{sample:?} {lines:?}");
                }
            }
        }
    }

    fn drive_sample(sample: Sample, output: OutputFormat) -> Vec<String> {
        let control = OperationControl::new(ExecutionPolicy::NonInteractive);
        let events = control.events();
        let cancel = control.cancellation();
        let log_path = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed = log_path.clone();
        let jsonl = output == OutputFormat::Jsonl;
        OUTPUT.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        if jsonl {
            emit_line(
                &serde_json::to_string(&InstallerEvent::started("app", "1.0.0", "install"))
                    .expect("started"),
            );
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let outcome = runtime.block_on(observe_execution(
            async move {
                match sample {
                    Sample::Quiet => Ok(InstallOutcome::Committed),
                    Sample::Lagged => {
                        for index in 0..400 {
                            let _ = events.send(RuntimeEvent::Progress {
                                completed: index,
                                total: 400,
                                action: "copy".into(),
                            });
                        }
                        Ok(InstallOutcome::Committed)
                    }
                    Sample::Cancelled => {
                        let _ = events.send(RuntimeEvent::Progress {
                            completed: 0,
                            total: 1,
                            action: "copy".into(),
                        });
                        tokio::task::yield_now().await;
                        cancel.cancel();
                        Ok(InstallOutcome::Cancelled)
                    }
                    Sample::Failed => Err(zup_runtime::SessionError::Transaction("disk".into())),
                    Sample::Reconciliation => Ok(InstallOutcome::CommittedPendingReconciliation {
                        detail: "ledger publish failed".into(),
                    }),
                    Sample::Committed => {
                        let _ = events.send(RuntimeEvent::Progress {
                            completed: 1,
                            total: 1,
                            action: "copy".into(),
                        });
                        Ok(InstallOutcome::Committed)
                    }
                }
            },
            &control,
            |event| note_progress(jsonl, &observed, event),
        ));
        let outcome = outcome.map_err(|error| miette::miette!("{error}"));
        let _ = publish_outcome(
            output,
            "app",
            "1.0.0",
            None,
            None,
            log_path.lock().expect("log").clone(),
            &[],
            outcome,
        );
        OUTPUT.with(|slot| slot.borrow_mut().take().unwrap_or_default())
    }
}
