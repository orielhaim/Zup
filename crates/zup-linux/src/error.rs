#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("target path `{path}` targets `{target}`, not Linux")]
    UnsupportedTarget { target: String, path: String },
    #[error("Linux path component `{component}` is invalid: {reason}")]
    InvalidComponent { component: String, reason: String },
    #[error(transparent)]
    Path(#[from] zup_platform::TargetPathError),
    #[error("cannot resolve {kind} `{path}`: {reason}")]
    Invalid {
        kind: &'static str,
        path: String,
        reason: String,
    },
    #[error("two owned resources claim `{second}`: {kind} collides with `{first}`")]
    Collision {
        kind: &'static str,
        first: String,
        second: String,
    },
    #[error("refused path `{path}`: {reason}")]
    Refused { path: String, reason: String },
    #[error("refused privileged path `{path}`: {reason}")]
    PolicyRefused { path: String, reason: String },
    #[error("machine state at `{path}`: {reason}")]
    StateRefused { path: String, reason: String },
    #[error("`{path}` is not a {expected}")]
    UnexpectedKind {
        path: String,
        expected: &'static str,
    },
    #[error("`{path}` already exists")]
    AlreadyExists { path: String },
    #[error("`{path}` does not exist")]
    Missing { path: String },
    #[error("i/o at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no home directory: `$HOME` is unset, so no user location can be resolved")]
    NoHome,
    #[error("this host reports no XDG state directory")]
    NoStateHome,
    #[error("no {location} location on Linux in {scope} scope: {reason}")]
    UnsupportedLocation {
        location: zup_core::InstallLocation,
        scope: &'static str,
        reason: &'static str,
    },
    #[error("the kernel reported the machine architecture `{0}`, which zup does not model")]
    UnknownArchitecture(String),
}

impl PathError {
    pub(crate) fn errno(path: &std::path::Path, error: rustix::io::Errno) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source: std::io::Error::from(error),
        }
    }

    pub(crate) fn io(path: &std::path::Path, error: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source: error,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("this installation cannot proceed on Linux: {reasons}")]
    Unsupported { reasons: String },
    #[error("{0}")]
    IntegrationUnsupported(String),
    #[error("cannot resolve for target `{target}`: not a Linux target")]
    UnsupportedTarget { target: String },
    #[error("location or template failure for {kind}: {source}")]
    Template {
        kind: &'static str,
        #[source]
        source: zup_platform::TemplateResolveError,
    },
    #[error("target path `{path}` is not a valid Linux path: {reason}")]
    InvalidTargetPath { path: String, reason: String },
    #[error("cannot transact {kind}: no Linux mechanism executes it in this phase")]
    UnsupportedKind { kind: &'static str },
    #[error(
        "file `{destination}` is {kind:?}: the planner could not decide, so there is no transaction to run"
    )]
    Undecided {
        destination: String,
        kind: zup_exec::FileOperationKind,
    },
    #[error("cannot transact service `{unit}`: {reason}")]
    Service { unit: String, reason: String },
    #[error("service `{id}` cannot become a systemd unit: {reason}")]
    ServiceRefused { id: String, reason: String },
    #[error("desktop entry name is empty")]
    EmptyName,
    #[error("desktop entry name `{name}` cannot be represented: {reason}")]
    InvalidName { name: String, reason: String },
    #[error("executable path is empty")]
    EmptyExecutable,
    #[error("executable or argument cannot be represented in `Exec=`: {reason}")]
    InvalidExec { reason: String },
    #[error("MIME type `{value}` is not a valid handler identifier")]
    InvalidMimeType { value: String },
    #[error("working directory is empty")]
    EmptyWorkingDirectory,
    #[error("MIME comment for `{mime}` cannot be represented: {reason}")]
    Comment { mime: String, reason: String },
    #[error("file extension `{extension}` is not a valid glob extension")]
    Extension { extension: String },
    #[error("MIME type `{value}` is not a valid custom type name")]
    Type { value: String },
    #[error(transparent)]
    Path(#[from] PathError),
}

impl PlanError {
    pub fn reasons(&self) -> &str {
        match self {
            Self::Unsupported { reasons } => reasons,
            _ => "",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("plan drift at `{path}`: {reason}")]
    PlanDrift { path: String, reason: String },
    #[error("verification failed at `{path}`: {reason}")]
    Verification { path: String, reason: String },
    #[error("rollback cannot restore `{path}`: {reason}")]
    RollbackDrift { path: String, reason: String },
    #[error("payload for `{path}` could not be staged: {source}")]
    Payload {
        path: String,
        #[source]
        source: zup_bundle::PayloadError,
    },
    #[error("service `{unit}`: {reason}")]
    Refused { unit: String, reason: String },
    #[error("service `{unit}` conflicts with existing state: {reason}")]
    Conflict { unit: String, reason: String },
    #[error("service `{unit}` drifted: {reason}")]
    Drift { unit: String, reason: String },
    #[error("service `{unit}` needs recovery: {reason}")]
    Ambiguous { unit: String, reason: String },
    #[error("systemd: {0}")]
    Systemd(#[from] IpcError),
    #[error("ledger JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ledger identity or schema mismatch")]
    LedgerInvalid,
    #[error("ledger publish requires a committed transaction")]
    LedgerUncommitted,
    #[error("transaction plan does not match committed ownership: {0}")]
    Ownership(String),
    #[error("unfinished transaction {0} requires recovery")]
    RecoveryRequired(String),
    #[error("cannot refresh derived integration state: `{tool}` is not available: {reason}")]
    RefreshUnavailable { tool: String, reason: String },
    #[error("`{tool}` failed for `{directory}`: {reason}")]
    RefreshFailed {
        tool: String,
        directory: String,
        reason: String,
    },
    #[error("refusing to refresh: {0}")]
    RefreshRefused(String),
    #[error("machine scope runs through the privileged worker, not in this process")]
    MachineScope,
    #[error("machine worker: {0}")]
    Worker(String),
    #[error("the confirmed plan went stale while the worker repaired state")]
    StalePlan,
    #[error("installer package: {0}")]
    Carrier(#[from] crate::carrier::CarrierError),
    #[error("package: {0}")]
    Package(#[from] zup_bundle::PackageError),
    #[error("plan: {0}")]
    Plan(#[from] zup_plan::PlanError),
    #[error("plan: {0}")]
    PlanFailure(#[from] PlanError),
    #[error("lock: {0}")]
    Lock(#[from] zup_transaction::LockError),
    #[error("lifecycle: {0}")]
    Lifecycle(#[from] zup_exec::LifecycleError),
    #[error("transaction plan: {0}")]
    Compile(#[from] zup_transaction::TransactionPlanError),
    #[error("transaction store: {0}")]
    Store(#[from] zup_transaction::StoreError),
    #[error("transaction executor: {0}")]
    Executor(String),
    #[error("transaction coordination: {0}")]
    Coordinator(#[from] zup_transaction::TransactionError),
    #[error("elevation: {0}")]
    Elevation(IpcError),
    #[error("downgrade from {installed} to {requested} is refused")]
    Downgrade {
        installed: semver::Version,
        requested: semver::Version,
    },
    #[error("an installer package holds exactly one target; this one holds {count}")]
    MultipleTargets { count: usize },
    #[error(transparent)]
    Path(#[from] PathError),
}

impl ExecError {
    pub(crate) fn refused(unit: &str, reason: impl Into<String>) -> Self {
        Self::Refused {
            unit: unit.to_owned(),
            reason: reason.into(),
        }
    }

    pub(crate) fn conflict(unit: &str, reason: impl Into<String>) -> Self {
        Self::Conflict {
            unit: unit.to_owned(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("systemd is unavailable: {0}")]
    SystemdUnavailable(String),
    #[error("systemd refused `{unit}`: {reason}")]
    SystemdRefused { unit: String, reason: String },
    #[error("systemd state for `{unit}` is ambiguous: {reason}")]
    SystemdAmbiguous { unit: String, reason: String },
    #[error("systemd operation timed out: {0}")]
    SystemdTimeout(String),
    #[error("no usable runtime directory: {0}")]
    NoRuntime(String),
    #[error("rendezvous I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("peer authentication failed: {0}")]
    PeerAuth(String),
    #[error("protocol framing failed: {0}")]
    Framing(String),
    #[error("handshake timed out")]
    Timeout,
    #[error("worker authentication failed: {0}")]
    WorkerAuth(String),
    #[error("worker protocol failed: {0}")]
    WorkerProtocol(String),
    #[error("worker policy refusal: {0}")]
    Policy(String),
    #[error("worker transaction failed: {0}")]
    Transaction(String),
    #[error("cancelled")]
    Cancelled,
    #[error("the confirmed plan went stale during preparation")]
    WorkerStalePlan,
    #[error("another operation holds this installation's lock")]
    WorkerBusy,
    #[error("pkexec is unavailable: {0}")]
    PkexecUnavailable(String),
    #[error("administrator authentication was cancelled")]
    PkexecCancelled,
    #[error("administrator authorization failed: {0}")]
    AuthorizationFailed(String),
    #[error("privileged worker failed: {0}")]
    PkexecWorkerFailed(String),
    #[error("privileged worker protocol failed: {0}")]
    PkexecProtocol(String),
    #[error("could not start pkexec at `{path}`: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl IpcError {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::WorkerAuth(_) => zup_protocol::failure::AUTHENTICATION,
            Self::WorkerProtocol(_) | Self::PkexecProtocol(_) | Self::Framing(_) => {
                zup_protocol::failure::PROTOCOL
            }
            Self::Policy(_) => zup_protocol::failure::POLICY,
            Self::Transaction(_) => zup_protocol::failure::TRANSACTION,
            Self::Cancelled | Self::PkexecCancelled => zup_protocol::failure::CANCELLED,
            Self::WorkerStalePlan => zup_protocol::failure::STALE_PLAN,
            Self::WorkerBusy => zup_protocol::failure::INSTALLATION_BUSY,
            _ => zup_protocol::failure::PROTOCOL,
        }
    }
}
