use zup_presentation::OutputFormat;
use zup_runtime::{InstallOutcome, PreparedOperation, RuntimeControl, runtime_request};

use crate::host::{SessionDiscovery, SessionHost};

pub async fn run_operation<H: SessionHost>(
    host: &H,
    discovery: &SessionDiscovery,
    prepared: &PreparedOperation,
    control: RuntimeControl,
) -> Result<InstallOutcome, zup_runtime::SessionError> {
    let backend = host.backend(prepared).map_err(session_error)?;
    let runtime = runtime_request(
        prepared,
        discovery.build.installer.app.id.clone(),
        prepared
            .target_plan
            .as_ref()
            .map(|target| target.app.version.clone())
            .or_else(|| discovery.prior().map(|ledger| ledger.version.clone()))
            .unwrap_or_else(|| discovery.build.installer.app.version.clone()),
        discovery.scope,
        discovery.state_root.clone(),
        zup_runtime::RequestOptions {
            bootstrap: prepared.bootstrap.clone(),
            release: None,
            recovery_id: None,
        },
    );
    zup_runtime::run_install_control_with_policy(
        &*backend,
        runtime,
        control.cancellation.clone(),
        control.events.clone(),
        control.policy,
    )
    .await
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
        InstallOutcome::Busy { .. } => "busy",
        InstallOutcome::Cancelled => "cancelled",
        InstallOutcome::RebootRequired { .. } => "reboot required",
        InstallOutcome::Failed(_) => "failed",
    }
}
