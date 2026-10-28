use std::path::{Path, PathBuf};

use zup_exec::LifecycleAction;
use zup_presentation::OutputFormat;
use zup_runtime::{InstallOutcome, OperationControl, PreparedOperation, runtime_request};

use crate::host::{OperationContext, SessionDiscovery, SessionHost, selection_of};

pub struct OperationCompletion {
    pub id: zup_runtime::Uuid,
    pub outcome: InstallOutcome,
    pub repair_drift: Vec<String>,
}

pub struct OperationLaunch<'a> {
    pub id: zup_runtime::Uuid,
    pub action: LifecycleAction,
    pub selection: &'a zup_preset_host::Selection,
    pub cleanup_lock: bool,
    pub control: OperationControl,
}

pub async fn complete_operation<H: SessionHost>(
    host: &H,
    executable: &Path,
    discovery: &SessionDiscovery,
    launch: OperationLaunch<'_>,
) -> OperationCompletion {
    let id = launch.id;
    let outcome = drive_operation(host, executable, discovery, launch).await;
    let repair_drift = outcome.1;
    OperationCompletion {
        id,
        outcome: outcome.0,
        repair_drift,
    }
}

async fn drive_operation<H: SessionHost>(
    host: &H,
    executable: &Path,
    discovery: &SessionDiscovery,
    launch: OperationLaunch<'_>,
) -> (InstallOutcome, Vec<String>) {
    let OperationLaunch {
        action,
        selection,
        cleanup_lock,
        control,
        ..
    } = launch;
    let context = match discovery.context(selection.scope) {
        Ok(context) => context,
        Err(message) => return (InstallOutcome::Failed(message), Vec::new()),
    };
    let (enable, disable) = selection_of(discovery.installer(), selection);
    let cancel = control.cancellation();
    let query = CancelQuery(&cancel);
    let prepared = {
        let bound = match host.bind(&context) {
            Ok(bound) => bound,
            Err(message) => return (InstallOutcome::Failed(message), Vec::new()),
        };
        let input = zup_runtime::OperationInput {
            build: &discovery.build,
            installer: executable,
            scope: context.scope,
            operation: zup_runtime::Operation::from(action),
            selection: zup_runtime::OperationSelection {
                enable,
                disable,
                install_directory: selection.install_directory.as_deref().map(PathBuf::from),
            },
            prior: context.ledger.as_ref(),
            state_root: &context.state_root,
            cancellation: &query,
        };
        match zup_runtime::prepare_operation(&input, &bound) {
            Ok(prepared) => prepared,
            Err(error) => return (InstallOutcome::Failed(error.to_string()), Vec::new()),
        }
    };
    let drifted = prepared
        .transaction_plan
        .retired_keys
        .iter()
        .map(|key| format!("{key:?}"))
        .collect::<Vec<_>>();
    let executed = run_operation(host, &context, discovery, &prepared, &control).await;
    let mut outcome = zup_runtime::outcome_of(executed);
    if cleanup_lock
        && matches!(outcome, InstallOutcome::Committed)
        && let Err(error) = host.after_commit(&context, &prepared)
    {
        outcome = InstallOutcome::CommittedPendingReconciliation { detail: error };
    }
    let drift = if matches!(action, LifecycleAction::Repair { .. })
        && matches!(outcome, InstallOutcome::Committed)
    {
        drifted
    } else {
        Vec::new()
    };
    (outcome, drift)
}

struct CancelQuery<'a>(&'a zup_runtime::CancellationHandle);

impl zup_plan::CancellationQuery for CancelQuery<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

pub async fn run_operation<H: SessionHost>(
    host: &H,
    context: &OperationContext,
    discovery: &SessionDiscovery,
    prepared: &PreparedOperation,
    control: &OperationControl,
) -> Result<InstallOutcome, zup_runtime::SessionError> {
    let backend = host.backend(context, prepared).map_err(session_error)?;
    let runtime = runtime_request(
        prepared,
        discovery.build.installer.app.id.clone(),
        prepared
            .target_plan()
            .map(|target| target.app.version.clone())
            .or_else(|| context.ledger.as_ref().map(|ledger| ledger.version.clone()))
            .unwrap_or_else(|| discovery.build.installer.app.version.clone()),
        context.scope,
        context.state_root.clone(),
        zup_runtime::RequestOptions {
            bootstrap: prepared.bootstrap().cloned(),
            release: host.release_identity(),
            recovery_id: None,
        },
    );
    zup_runtime::execute(&*backend, runtime, control).await
}

fn session_error(message: String) -> zup_runtime::SessionError {
    zup_runtime::SessionError::Transaction(message)
}

pub struct Reporter {
    output: OutputFormat,
}

impl Reporter {
    pub fn new(output: OutputFormat) -> Self {
        Self { output }
    }

    pub fn emit(&self, event: &zup_runtime::RuntimeEvent) {
        if self.output == OutputFormat::Human {
            return;
        }
        for automation in crate::host::automation_events(event) {
            match serde_json::to_string(&automation) {
                Ok(value) => println!("{value}"),
                Err(_) => return,
            }
        }
    }

    pub fn finish(&self, outcome: &InstallOutcome, action: &str) {
        let process = crate::host::process_outcome(outcome);
        match self.output {
            OutputFormat::Human => {
                println!("{action} {}", process_human(outcome));
            }
            OutputFormat::Json | OutputFormat::Jsonl => {
                let event = zup_presentation::InstallerEvent::Completed { outcome: process };
                if let Ok(value) = serde_json::to_string(&event) {
                    println!("{value}");
                }
            }
        }
    }
}

fn process_human(outcome: &InstallOutcome) -> &'static str {
    match outcome {
        InstallOutcome::Committed => "committed",
        InstallOutcome::RolledBack => "rolled back",
        InstallOutcome::RecoveryRequired => "requires recovery",
        InstallOutcome::CommittedPendingReconciliation { .. } => {
            "committed, reconciliation required"
        }
        InstallOutcome::AuthorizationRequired => "authorization required",
        InstallOutcome::AuthorizationCancelled => "authorization cancelled",
        InstallOutcome::Busy { .. } => "busy",
        InstallOutcome::Cancelled => "cancelled",
        InstallOutcome::RebootRequired { .. } => "reboot required",
        InstallOutcome::Failed(_) => "failed",
    }
}
