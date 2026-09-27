//! Recovery from an interrupted transaction.
//!
//! Recovery is a capability, not a command. The engine reconciles a transaction
//! whose record exists but whose work stopped — because the machine lost power,
//! because a prerequisite demanded a reboot, because a worker was killed — and it
//! does so from the record it wrote before it started. A person should not have
//! to read a transaction identifier out of a log and type it into a setup file to
//! get their machine back.
//!
//! The one thing this module provides is the explicit form, for the two callers
//! that legitimately need it: an automation system reconciling on a schedule, and
//! an internal handoff that already knows which transaction it is looking at. It
//! is hidden from help, because an identifier in a help screen is an identifier
//! somebody will start depending on.

use std::path::PathBuf;

use clap::Args;
use zup_core::SelectedScope;
use zup_exec::LifecycleAction;
use zup_presentation::OutputFormat;
use zup_runtime::RuntimeRequest;
use zup_transaction::{NodeKind, TransactionId, TransactionStore};

use crate::cli::{OutputArg, ScopeArg};
use crate::context::RuntimeContext;
use crate::execute;
use crate::lifecycle::PreparedRuntime;
use crate::package;
use crate::state;

/// Reconcile one interrupted transaction.
#[derive(Debug, Args)]
pub struct RecoveryArgs {
    /// The transaction to reconcile.
    #[arg(long)]
    pub transaction_id: uuid::Uuid,
    /// Which installation the transaction belongs to.
    #[arg(long, value_enum, default_value = "user")]
    pub scope: ScopeArg,
    /// Answer with a machine-readable result instead of prose.
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputArg,

    /// The state root the transaction record lives in. Derived when absent.
    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// Where the payload files for a staged transaction are.
    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub payload_root: Option<PathBuf>,
    /// The scratch directory the transaction used.
    #[arg(long, hide = true, value_hint = clap::ValueHint::DirPath)]
    pub work_root: Option<PathBuf>,
}

/// Reconcile one interrupted transaction.
pub fn run(context: RuntimeContext, args: RecoveryArgs) -> miette::Result<()> {
    let _ = context;
    let executable = package::current_executable()?;
    let bundle = package::open_bundle_if_present(&executable)?;
    let embedded = bundle.as_ref().map(package::target_plan).transpose()?;
    let scope = embedded.as_ref().map_or_else(
        || SelectedScope::from(args.scope),
        |build| state::default_install_scope(build.installer.install.scope),
    );
    let state_root = state::resolve_state_root(args.state_root, scope)?;
    let id = TransactionId::from_uuid(args.transaction_id);
    let record = zup_transaction::FilesystemTransactionStore::new(&state_root)
        .load(&id)
        .map_err(|error| miette::miette!("transaction: {error}"))?;
    // A transaction that staged files needs the bytes it staged. They are either
    // beside this image, which is the ordinary case, or wherever the caller says
    // they are. There is no third answer: reconciling a file transaction without
    // its payload would restore nothing and report success.
    let stages_files = record
        .plan
        .nodes
        .iter()
        .any(|node| matches!(node.kind, NodeKind::StageFile { .. }));
    if stages_files && args.payload_root.is_none() && bundle.is_none() {
        return Err(miette::miette!(
            "a file transaction needs the payload it staged; pass --payload-root"
        ));
    }
    let work_root = args.work_root.unwrap_or_else(|| state_root.join("work"));
    let payload_root = args.payload_root.unwrap_or_else(|| {
        if bundle.is_some() {
            executable
        } else {
            std::env::current_dir().unwrap_or_else(|_| state_root.clone())
        }
    });
    let target = embedded
        .as_ref()
        .map(|build| build.installer.target.clone())
        .unwrap_or_else(|| record.target.clone());
    if target != record.target {
        return Err(miette::miette!(
            "this package targets `{target}` but the transaction targets `{}`",
            record.target
        ));
    }
    let backend = zup_windows::WindowsRuntimeBackend::for_recovery(
        payload_root,
        &state_root,
        record.scope,
        id,
    )
    .map_err(|error| miette::miette!("payload recovery: {error}"))?;
    let request = RuntimeRequest {
        target,
        app_id: record.app_id.clone(),
        app_version: record.app_version,
        scope: record.scope,
        transaction_plan: record.plan.clone(),
        state_root,
        work_root,
        recovery_id: Some(id),
        bootstrap: None,
        release: None,
    };
    let prepared = PreparedRuntime {
        request,
        backend: std::sync::Arc::new(backend),
        action: LifecycleAction::Repair { force_files: false },
    };
    if OutputFormat::from(args.output) == OutputFormat::Human {
        execute::execute(prepared)
    } else {
        execute::execute_frontend(
            prepared,
            OutputFormat::from(args.output),
            LifecycleAction::Repair { force_files: false },
        )
        .map_err(Into::into)
    }
}
