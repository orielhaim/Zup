use std::io::Write;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use zup_core::{Frontend, SelectedScope};
use zup_presentation::{
    InstallerEvent, InstallerResult, OutputFormat, PlanPreview, ProcessOutcome,
};
use zup_runtime::{
    ExecutionPolicy, InstallOutcome, Operation, OperationControl, OperationInput,
    OperationSelection,
};
use zup_session::SessionHost;

#[derive(Debug, Parser)]
#[command(
    name = "setup",
    version,
    about = "Install and maintain this application."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) operation: Option<Command>,
    #[arg(long, global = true, value_enum)]
    pub(crate) scope: Option<Scope>,
    #[arg(long, alias = "install-dir", global = true, value_name = "PATH")]
    pub(crate) install_directory: Option<PathBuf>,
    #[arg(long, global = true, hide = true)]
    pub(crate) state_root: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "human")]
    pub(crate) output: Output,
    #[arg(long, global = true)]
    pub(crate) non_interactive: bool,
    #[arg(long, global = true)]
    pub(crate) yes: bool,
    /// Use line prompts without terminal effects, suitable for screen readers.
    #[arg(long, global = true)]
    pub(crate) plain: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    Install(Components),
    Upgrade(Components),
    Modify(Components),
    Repair {
        #[arg(long, alias = "force")]
        force_files: bool,
    },
    Uninstall,
}

#[derive(Debug, Default, Args)]
pub(crate) struct Components {
    #[arg(long, value_name = "ID")]
    pub(crate) enable: Vec<String>,
    #[arg(long, value_name = "ID")]
    pub(crate) disable: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum Scope {
    User,
    Machine,
}
impl From<Scope> for SelectedScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::User => Self::User,
            Scope::Machine => Self::Machine,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub(crate) enum Output {
    #[default]
    Human,
    Json,
    Jsonl,
}
impl From<Output> for OutputFormat {
    fn from(output: Output) -> Self {
        match output {
            Output::Human => Self::Human,
            Output::Json => Self::Json,
            Output::Jsonl => Self::Jsonl,
        }
    }
}

impl Cli {
    fn requested_operation(&self) -> Operation {
        match &self.operation {
            Some(Command::Install(_)) => Operation::Install,
            Some(Command::Upgrade(_)) => Operation::Upgrade,
            Some(Command::Modify(_)) => Operation::Modify,
            Some(Command::Repair { force_files }) => Operation::Repair {
                force_files: *force_files,
            },
            Some(Command::Uninstall) => Operation::Uninstall,
            None => Operation::Apply,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct Failure {
    pub outcome: ProcessOutcome,
    pub message: String,
}
impl miette::Diagnostic for Failure {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new(
            format!("zup_installer::{:?}", self.outcome).to_ascii_lowercase(),
        ))
    }
}
impl From<zup_runtime::OperationError> for Failure {
    fn from(error: zup_runtime::OperationError) -> Self {
        let message = error.to_string();
        let inferred = ProcessOutcome::from_message(&message);
        let outcome = if inferred == ProcessOutcome::Failure {
            match error {
                zup_runtime::OperationError::Downgrade { .. } => ProcessOutcome::Configuration,
                zup_runtime::OperationError::Refused {
                    stage: "scope" | "selection" | "capabilities" | "plan" | "resolve",
                    ..
                } => ProcessOutcome::Configuration,
                _ => inferred,
            }
        } else {
            inferred
        };
        Self { outcome, message }
    }
}

pub fn command() -> clap::Command {
    Cli::command()
}

pub(crate) fn parse() -> Cli {
    Cli::try_parse().unwrap_or_else(|error| {
        let code = if error.exit_code() == 0 {
            0
        } else {
            ProcessOutcome::InvalidInvocation.code()
        };
        let _ = error.print();
        std::process::exit(code)
    })
}

#[cfg(windows)]
pub fn uninstall<H: SessionHost>(
    host: H,
    context: crate::run::RuntimeContext,
    scope: Option<SelectedScope>,
    state_root: Option<PathBuf>,
    non_interactive: bool,
    yes: bool,
    plain: bool,
) -> miette::Result<()> {
    run_cli(
        host,
        context.frontend,
        Cli {
            operation: Some(Command::Uninstall),
            scope: scope.map(|scope| match scope {
                SelectedScope::User => Scope::User,
                SelectedScope::Machine => Scope::Machine,
            }),
            state_root,
            install_directory: None,
            non_interactive,
            yes,
            plain,
            output: match context.output {
                OutputFormat::Human => Output::Human,
                OutputFormat::Json => Output::Json,
                OutputFormat::Jsonl => Output::Jsonl,
            },
        },
    )
}

pub(crate) fn run_cli<H: SessionHost>(
    mut host: H,
    frontend: Frontend,
    cli: Cli,
) -> miette::Result<()> {
    let output = cli.output.into();
    let interactive = frontend == Frontend::Console
        && output == OutputFormat::Human
        && !cli.non_interactive
        && !cli.yes
        && crate::run::console_is_a_terminal();
    let mut report = Report::new(
        output,
        interactive,
        cli.plain,
        cli.requested_operation().name(),
    );

    let result = drive(&mut host, &cli, interactive, &mut report);
    let outcome = match &result {
        Ok(outcome) => zup_session::process_outcome(outcome),
        Err(error) => error
            .downcast_ref::<Failure>()
            .map(|error| error.outcome)
            .unwrap_or_else(|| ProcessOutcome::from_message(&error.to_string())),
    };
    let message = match &result {
        Err(error) => Some(error.to_string()),
        Ok(InstallOutcome::Failed(message)) => Some(message.clone()),
        Ok(InstallOutcome::CommittedPendingReconciliation { detail }) => Some(detail.clone()),
        Ok(InstallOutcome::Busy { .. }) => Some("another maintenance operation is running".into()),
        _ => None,
    };
    report.result.outcome = outcome;
    report.result.code = outcome.code();
    report.result.message = match (message, report.result.message.take()) {
        (Some(message), Some(detail)) if !message.contains(&detail) => {
            Some(format!("{message}\n{detail}"))
        }
        (message, detail) => message.or(detail),
    };
    report.finish()?;
    if outcome == ProcessOutcome::Success {
        Ok(())
    } else {
        Err(Failure {
            outcome,
            message: report
                .result
                .message
                .unwrap_or_else(|| format!("{outcome:?}")),
        }
        .into())
    }
}

fn drive<H: SessionHost>(
    host: &mut H,
    cli: &Cli,
    interactive: bool,
    report: &mut Report,
) -> miette::Result<InstallOutcome> {
    let executable =
        std::env::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let mut build = host
        .describe(&executable)
        .map_err(|error| miette::miette!("{error}"))?;
    let installer = &build.installer;
    report.result.application = installer.app.name.to_string();
    report.result.version = installer.app.version.to_string();
    report.event(&InstallerEvent::started(
        &report.result.application,
        &report.result.version,
        report.action,
    ))?;
    #[cfg(feature = "console")]
    if interactive {
        crate::console::Console::new(cli.plain).intro(installer)?;
    }
    let scope = choose_scope(installer, cli.scope.map(Into::into), interactive, cli.plain)?;
    report.result.scope = Some(scope);
    let context = host
        .observe(installer, scope, cli.state_root.as_deref())
        .map_err(|error| miette::miette!("{error}"))?;
    if cli.operation.is_none() && !interactive {
        return Err(Failure {
            outcome: ProcessOutcome::InvalidInvocation,
            message: "choose an operation: install, upgrade, modify, repair, or uninstall".into(),
        }
        .into());
    }
    let operation = cli.requested_operation();
    #[cfg(feature = "console")]
    let operation = if interactive
        && cli.operation.is_none()
        && let Some(ledger) = &context.ledger
    {
        crate::console::Console::new(cli.plain).maintenance(
            &zup_runtime::maintenance_operations(&ledger.version, &installer.app.version),
        )?
    } else {
        operation
    };
    #[cfg(feature = "console")]
    let action = zup_runtime::resolve_operation(
        operation,
        context.ledger.as_ref().map(|ledger| &ledger.version),
        &installer.app.version,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    report.event(&InstallerEvent::Phase {
        state: "planning".into(),
    })?;
    let (enable, disable) = match &cli.operation {
        Some(Command::Install(args) | Command::Upgrade(args) | Command::Modify(args)) => {
            (args.enable.clone(), args.disable.clone())
        }
        _ => Default::default(),
    };
    let selection = OperationSelection {
        enable,
        disable,
        install_directory: cli.install_directory.clone(),
    };
    #[cfg(feature = "console")]
    let selection = {
        let mut selection = selection;
        if interactive {
            crate::console::Console::new(cli.plain).selection(
                installer,
                &context,
                action,
                &mut selection,
            )?;
        }
        selection
    };
    report.event(&InstallerEvent::Phase {
        state: "checking_content".into(),
    })?;
    let mut reporting_error = None;
    host.acquire_content(&mut build, &context, operation, &selection, &mut |event| {
        if !matches!(
            event,
            InstallerEvent::Completed { .. } | InstallerEvent::Failed { .. }
        ) && let Err(error) = report.event(&event)
        {
            reporting_error = Some(error);
        }
    })
    .map_err(|error| miette::miette!("content acquisition: {error}"))?;
    if let Some(error) = reporting_error {
        return Err(error);
    }
    report.result.application = build.installer.app.name.to_string();
    report.result.version = build.installer.app.version.to_string();
    let bound = host
        .bind(&context)
        .map_err(|error| miette::miette!("{error}"))?;
    let prepared = zup_runtime::prepare_operation(
        &OperationInput {
            build: &build,
            installer: &executable,
            scope,
            operation,
            selection,
            prior: context.ledger.as_ref(),
            state_root: &context.state_root,
            cancellation: &zup_plan::NeverCancelled,
        },
        &bound,
    )
    .map_err(Failure::from)?;
    let mut plan = PlanPreview::from_transaction_plan(&prepared.transaction_plan, scope);
    plan.application = build.installer.app.name.to_string();
    plan.version = build.installer.app.version.to_string();
    if let Some(target) = prepared.target_plan() {
        plan.install_directory = target.install_directory.to_string();
        plan.selected_components = target.selected_components.clone();
    } else if let Some(ledger) = &context.ledger {
        plan.install_directory = ledger
            .install_directory
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        plan.selected_components = ledger.selected_components.clone();
    }
    report.result.install_directory =
        (!plan.install_directory.is_empty()).then(|| plan.install_directory.clone());
    #[cfg(feature = "console")]
    if interactive {
        crate::console::Console::new(cli.plain).review(&plan, prepared.action)?;
    }
    let control = OperationControl::new(if interactive {
        ExecutionPolicy::Interactive
    } else {
        ExecutionPolicy::NonInteractive
    });
    let mut events = control.subscribe();
    tokio::runtime::Builder::new_current_thread().enable_all().build()
        .map_err(|error| miette::miette!("runtime: {error}"))?.block_on(async {
            let execute = zup_session::run_operation(host, &context, &build, &prepared, &control);
            tokio::pin!(execute);
            let interrupted = tokio::signal::ctrl_c();
            tokio::pin!(interrupted);
            let mut cancelling = false;
            let mut reporting_error = None;
            let executed = loop {
                tokio::select! {
                    result = &mut execute => break result,
                    signal = &mut interrupted, if !cancelling => {
                        if let Err(error) = signal { reporting_error = Some(miette::miette!("cancellation handler: {error}")); }
                        cancelling = true;
                        control.cancellation().cancel();
                        if let Err(error) = report.event(&InstallerEvent::Cancelling { state: "safe_boundary".into() }) { reporting_error = Some(error); }
                    }
                    event = events.recv() => if let Ok(event) = event && let Err(error) = report.runtime_event(&event) { reporting_error = Some(error); control.cancellation().cancel(); },
                }
            };
            while let Ok(event) = events.try_recv() { if let Err(error) = report.runtime_event(&event) { reporting_error = Some(error); } }
            let (outcome, drift) = zup_session::reconcile_operation(host, &context, &prepared, zup_runtime::outcome_of(executed));
            report.result.drift = drift;
            if matches!(outcome, InstallOutcome::RecoveryRequired | InstallOutcome::CommittedPendingReconciliation { .. }) {
                let details = match zup_runtime::discover_recovery(&context.state_root) {
                    Ok(states) => states.into_iter().filter_map(|state| match state {
                        zup_runtime::RecoveryStatus::RecoveryRequired { transaction_id }
                        | zup_runtime::RecoveryStatus::PreparedTransaction { transaction_id }
                        | zup_runtime::RecoveryStatus::InterruptedApplyingTransaction { transaction_id } => Some(transaction_id),
                        zup_runtime::RecoveryStatus::CommittedTransaction { transaction_id } if matches!(outcome, InstallOutcome::CommittedPendingReconciliation { .. }) => Some(transaction_id),
                        _ => None,
                    }).collect::<Vec<_>>().join(", "),
                    Err(error) => format!("cannot inspect transaction state: {error}"),
                };
                let message = report.result.message.get_or_insert_with(String::new);
                message.push_str(&format!("\nTransaction state: {}\n{details}", context.state_root.display()));
            }
            if let Some(error) = reporting_error { return Err(error); }
            Ok(outcome)
        })
}

fn choose_scope(
    installer: &zup_core::Installer,
    requested: Option<SelectedScope>,
    interactive: bool,
    _plain: bool,
) -> miette::Result<SelectedScope> {
    let declared = installer.install.scope;
    if requested.is_some() || !interactive || declared.scopes().len() == 1 {
        return declared.select(requested).map_err(|error| {
            miette::Report::new(Failure {
                outcome: ProcessOutcome::InvalidInvocation,
                message: format!("{error}; specify --scope user or --scope machine"),
            })
        });
    }
    #[cfg(feature = "console")]
    return crate::console::Console::new(_plain).scope(installer);
    #[cfg(not(feature = "console"))]
    Err(miette::miette!("this runtime has no interactive frontend"))
}

struct Report {
    output: OutputFormat,
    started: bool,
    action: &'static str,
    result: InstallerResult,
    #[cfg(feature = "console")]
    progress: Option<crate::console::Progress>,
}
impl Report {
    fn new(output: OutputFormat, _interactive: bool, _plain: bool, action: &'static str) -> Self {
        Self {
            output,
            started: false,
            action,
            result: InstallerResult::new(ProcessOutcome::Failure, "", ""),
            #[cfg(feature = "console")]
            progress: _interactive.then(|| crate::console::Progress::new(_plain)),
        }
    }
    fn event(&mut self, event: &InstallerEvent) -> miette::Result<()> {
        if matches!(event, InstallerEvent::Started { .. }) {
            self.started = true;
        }
        if self.output == OutputFormat::Human
            && let InstallerEvent::Phase { state } = event
        {
            #[cfg(feature = "console")]
            let rendered = self.progress.is_some();
            #[cfg(not(feature = "console"))]
            let rendered = false;
            if !rendered && !state.starts_with("operation:") && !state.starts_with("staging:") {
                eprintln!("  {}", state.replace('_', " "));
            }
        }
        if self.output == OutputFormat::Jsonl {
            write_json(event)?;
        }
        #[cfg(feature = "console")]
        if let Some(progress) = &mut self.progress {
            progress.event(event);
        }
        Ok(())
    }
    fn runtime_event(&mut self, event: &zup_runtime::RuntimeEvent) -> miette::Result<()> {
        if let zup_runtime::RuntimeEvent::Failed { message, .. }
        | zup_runtime::RuntimeEvent::ResourceBlocked {
            detail: message, ..
        } = event
        {
            match &mut self.result.message {
                Some(detail) if !detail.contains(message) => {
                    detail.push('\n');
                    detail.push_str(message);
                }
                None => self.result.message = Some(message.clone()),
                _ => {}
            }
        }

        if let zup_runtime::RuntimeEvent::LogPath { path } = event {
            self.result.log_path = Some(path.clone());
        }
        for event in zup_session::automation_events(event) {
            if !matches!(
                event,
                InstallerEvent::Completed { .. } | InstallerEvent::Failed { .. }
            ) {
                self.event(&event)?;
            }
        }
        Ok(())
    }
    fn finish(&mut self) -> miette::Result<()> {
        if !self.started {
            self.event(&InstallerEvent::started(
                &self.result.application,
                &self.result.version,
                self.action,
            ))?;
        }
        #[cfg(feature = "console")]
        if let Some(progress) = &mut self.progress {
            progress.finish();
        }
        match self.output {
            OutputFormat::Json => write_json(&self.result)?,
            OutputFormat::Jsonl => {
                let event = if self.result.outcome == ProcessOutcome::Success {
                    InstallerEvent::Completed {
                        outcome: self.result.outcome,
                    }
                } else {
                    InstallerEvent::Failed {
                        outcome: self.result.outcome,
                        code: self.result.code,
                        message: self
                            .result
                            .message
                            .clone()
                            .unwrap_or_else(|| format!("{:?}", self.result.outcome)),
                        diagnostic: Some(zup_presentation::DiagnosticPresentation::from_message(
                            self.result
                                .message
                                .as_deref()
                                .unwrap_or("operation incomplete"),
                            self.result.outcome == ProcessOutcome::RecoveryRequired,
                        )),
                    }
                };
                self.event(&event)?;
            }
            OutputFormat::Human => {
                if self.result.outcome == ProcessOutcome::Success {
                    println!("Completed successfully.");
                } else if self.result.outcome == ProcessOutcome::Cancelled {
                    eprintln!("Cancelled safely.");
                } else {
                    let message = self
                        .result
                        .message
                        .as_deref()
                        .unwrap_or("The operation did not complete.");
                    eprintln!("{message}");
                    let diagnostic = zup_presentation::DiagnosticPresentation::from_message(
                        message,
                        self.result.outcome == ProcessOutcome::RecoveryRequired,
                    );
                    eprintln!("{}", diagnostic.recovery);
                    if let Some(log) = &self.result.log_path {
                        eprintln!("Log: {log}");
                    }
                }
            }
        }
        Ok(())
    }
}

fn write_json(value: &impl serde::Serialize) -> miette::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(|error| miette::miette!("output: {error}"))?;
    line.push(b'\n');
    std::io::stdout()
        .lock()
        .write_all(&line)
        .map_err(|error| miette::miette!("output: {error}"))
}
