use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zup_core::{ComponentId, Installer, SelectedScope};
use zup_exec::LifecycleAction;
use zup_presentation::{InstallerEvent, PlanPreview};
use zup_runtime::{Operation, OperationSelection};
use zup_session::{OperationContext, SessionDiscovery};

fn prompt<T>(result: io::Result<T>) -> miette::Result<T> {
    result.map_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted {
            crate::terminal::Failure {
                outcome: zup_presentation::ProcessOutcome::Cancelled,
                message: "cancelled".into(),
            }
            .into()
        } else {
            miette::miette!("terminal input: {error}")
        }
    })
}

fn text(value: impl std::fmt::Display) -> String {
    console::strip_ansi_codes(&value.to_string())
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn limited_terminal() -> bool {
    std::env::var("TERM").is_ok_and(|term| term == "dumb") || console::Term::stderr().size().1 < 50
}

#[derive(Clone, Copy)]
pub struct Console {
    plain: bool,
}

impl Console {
    pub fn new(plain: bool) -> Self {
        Self { plain }
    }
    fn plain(&self) -> bool {
        self.plain || limited_terminal()
    }

    fn line(&self, question: &str) -> miette::Result<String> {
        eprint!("{question}: ");
        prompt(io::stderr().flush())?;
        let mut value = String::new();
        if prompt(io::stdin().read_line(&mut value))? == 0 {
            return Err(crate::terminal::Failure {
                outcome: zup_presentation::ProcessOutcome::Cancelled,
                message: "input closed; cancelled".into(),
            }
            .into());
        }
        Ok(value.trim().to_owned())
    }

    fn choose(&self, question: &str, items: &[(String, String)]) -> miette::Result<usize> {
        if self.plain() {
            eprintln!("{question}");
            for (index, (label, hint)) in items.iter().enumerate() {
                eprintln!("  {}. {label} {hint}", index + 1);
            }
            loop {
                let value = self.line("Choose a number, or q to cancel")?;
                if value.eq_ignore_ascii_case("q") {
                    return Err(cancelled());
                }
                if let Ok(index) = value.parse::<usize>()
                    && index > 0
                    && index <= items.len()
                {
                    return Ok(index - 1);
                }
                eprintln!("Enter a number between 1 and {}.", items.len());
            }
        }
        let mut select = cliclack::select(text(question)).max_rows(8);
        for (index, (label, hint)) in items.iter().enumerate() {
            select = select.item(index, text(label), text(hint));
        }
        prompt(select.interact())
    }

    pub fn intro(&self, installer: &Installer) -> miette::Result<()> {
        let title = format!("{} {}", text(&installer.app.name), installer.app.version);
        if self.plain() {
            eprintln!("\n{title}\n");
        } else {
            prompt(cliclack::intro(title))?;
        }
        if let Some(description) = &installer.app.description {
            eprintln!("{}\n", text(description));
        }
        Ok(())
    }

    pub fn scope(&self, discovery: &SessionDiscovery) -> miette::Result<SelectedScope> {
        let scopes = discovery.installer().install.scope.scopes();
        let items = scopes
            .iter()
            .map(|scope| {
                let label = match scope {
                    SelectedScope::User => "Current user",
                    SelectedScope::Machine => "All users",
                };
                let installed = discovery
                    .installation(*scope)
                    .is_some_and(|installation| installation.ledger.is_some());
                (
                    label.into(),
                    if installed {
                        "Existing installation".into()
                    } else if *scope == SelectedScope::Machine {
                        "Requires native administrator authorization".into()
                    } else {
                        String::new()
                    },
                )
            })
            .collect::<Vec<_>>();
        Ok(scopes[self.choose("Install for", &items)?])
    }

    pub fn maintenance(&self, operations: &[Operation]) -> miette::Result<Operation> {
        let items = operations
            .iter()
            .map(|operation| match operation {
                Operation::Upgrade => ("Upgrade", "Install the newer version in this package"),
                Operation::Modify => (
                    "Change components",
                    "Keep the current installation settings",
                ),
                Operation::Repair { .. } => (
                    "Repair",
                    "Restore owned files without overwriting your edits",
                ),
                Operation::Uninstall => (
                    "Uninstall",
                    "Remove managed resources and preserve unowned files",
                ),
                Operation::Install | Operation::Apply => ("Install", "Install this package"),
            })
            .map(|(label, hint)| (label.into(), hint.into()))
            .collect::<Vec<_>>();
        if operations.len() == 1 {
            return Ok(operations[0]);
        }
        Ok(operations[self.choose("Maintenance", &items)?])
    }

    pub fn selection(
        &self,
        installer: &Installer,
        context: &OperationContext,
        action: LifecycleAction,
        selection: &mut OperationSelection,
    ) -> miette::Result<()> {
        if matches!(
            action,
            LifecycleAction::Repair { .. } | LifecycleAction::Uninstall
        ) {
            return Ok(());
        }
        let request = zup_runtime::plan_request(
            installer,
            context.scope,
            context.ledger.as_ref(),
            selection,
            action,
            None,
        )
        .map_err(|error| miette::miette!("{error}"))?;
        let mut selected = zup_plan::select_components(&installer.components, &request.components)
            .map_err(|error| miette::miette!("{error}"))?;
        let required = installer
            .components
            .iter()
            .filter(|component| component.required)
            .map(|component| text(&component.name))
            .collect::<Vec<_>>();
        if !required.is_empty() {
            eprintln!("Required: {}", required.join(", "));
        }
        let mut groups = installer
            .component_groups
            .iter()
            .map(|group| {
                (
                    Some(group.id.as_str()),
                    group
                        .label
                        .as_ref()
                        .map(|label| label.as_str())
                        .unwrap_or(group.id.as_str()),
                    group.description.as_deref(),
                    group.selection == zup_core::SelectionRequirement::Explicit,
                )
            })
            .collect::<Vec<_>>();
        groups.push((None, "Components", None, false));
        for (id, label, description, required) in groups
            .into_iter()
            .filter(|_| selection.enable.is_empty() && selection.disable.is_empty())
        {
            let optional = installer
                .components
                .iter()
                .filter(|component| {
                    !component.required
                        && component.group.as_ref().map(|group| group.as_str()) == id
                })
                .collect::<Vec<_>>();
            if optional.is_empty() {
                continue;
            }
            if let Some(description) = description {
                eprintln!("{}", text(description));
            }
            let chosen = if self.plain() {
                let mut chosen = Vec::new();
                for component in &optional {
                    if self.confirm(
                        &format!("Include {}?", text(&component.name)),
                        selected.contains(&component.id),
                    )? {
                        chosen.push(component.id.clone());
                    }
                }
                chosen
            } else {
                let mut select = cliclack::multiselect(text(label))
                    .required(required)
                    .max_rows(8)
                    .filter_mode();
                for component in &optional {
                    select = select.item(
                        component.id.clone(),
                        text(&component.name),
                        text(component.description.as_deref().unwrap_or("")),
                    );
                }
                prompt(
                    select
                        .initial_values(
                            selected
                                .iter()
                                .filter(|id| optional.iter().any(|component| &component.id == *id))
                                .cloned()
                                .collect(),
                        )
                        .interact(),
                )?
            };
            selected.retain(|id| !optional.iter().any(|component| &component.id == id));
            selected.extend(chosen);
        }
        selection.enable = selected.iter().map(ToString::to_string).collect();
        selection.disable = installer
            .components
            .iter()
            .filter(|component| !selected.contains(&component.id))
            .map(|component| component.id.to_string())
            .collect();
        if installer.install.allow_directory_override
            && context.ledger.is_none()
            && selection.install_directory.is_none()
        {
            let value = if self.plain() {
                self.line("Install directory, or Enter for the configured location")?
            } else {
                prompt(
                    cliclack::input("Install directory")
                        .placeholder("Enter to use the configured location")
                        .required(false)
                        .interact::<String>(),
                )?
            };
            if !value.is_empty() {
                selection.install_directory = Some(PathBuf::from(value));
            }
        }
        Ok(())
    }

    pub fn review(&self, plan: &PlanPreview, action: LifecycleAction) -> miette::Result<()> {
        eprintln!(
            "\n{}\n  Scope: {}\n  Location: {}",
            zup_runtime::Operation::from(action).name(),
            plan.scope,
            text(&plan.install_directory)
        );
        if !plan.selected_components.is_empty() {
            eprintln!(
                "  Components: {}",
                plan.selected_components
                    .iter()
                    .map(ComponentId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for group in &plan.groups {
            eprintln!("  {:?}: {} changes", group.category, group.changes.len());
        }
        eprintln!(
            "  Managed content: {}",
            indicatif::HumanBytes(plan.estimated_bytes)
        );
        if matches!(action, LifecycleAction::Repair { force_files: true }) {
            eprintln!("  Forced repair can replace your edits to managed files.");
        }
        if plan.requires_authorization {
            eprintln!("  The operating system will request administrator authorization.");
        }
        if !self.confirm("Proceed with these changes?", false)? {
            return Err(cancelled());
        }
        Ok(())
    }

    pub fn confirm(&self, question: &str, initial: bool) -> miette::Result<bool> {
        if self.plain() {
            loop {
                let value = self.line(&format!(
                    "{} {}",
                    text(question),
                    if initial { "[Y/n/q]" } else { "[y/N/q]" }
                ))?;
                match value.to_ascii_lowercase().as_str() {
                    "" => return Ok(initial),
                    "y" | "yes" => return Ok(true),
                    "n" | "no" => return Ok(false),
                    "q" => return Err(cancelled()),
                    _ => eprintln!("Enter y or n, or q to cancel."),
                }
            }
        }
        prompt(
            cliclack::confirm(text(question))
                .initial_value(initial)
                .interact(),
        )
    }
}

pub fn cancelled() -> miette::Report {
    crate::terminal::Failure {
        outcome: zup_presentation::ProcessOutcome::Cancelled,
        message: "cancelled".into(),
    }
    .into()
}

pub struct Progress {
    bar: indicatif::ProgressBar,
    last: Instant,
    console: Console,
}
impl Progress {
    pub fn new(plain: bool) -> Self {
        let console = Console::new(plain);
        let bar = if console.plain() {
            indicatif::ProgressBar::hidden()
        } else {
            indicatif::ProgressBar::with_draw_target(
                None,
                indicatif::ProgressDrawTarget::stderr_with_hz(10),
            )
        };
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "  {wide_msg:.cyan} {percent:>3}% ({pos}/{len})",
            )
            .expect("static progress template"),
        );
        Self {
            bar,
            last: Instant::now(),
            console,
        }
    }
    pub fn event(&mut self, event: &InstallerEvent) {
        match event {
            InstallerEvent::Progress {
                completed,
                total,
                label,
                ..
            } => {
                self.bar.set_length(*total);
                self.bar.set_position(*completed);
                self.bar.set_message(text(label));
                if self.console.plain() && self.last.elapsed() >= Duration::from_secs(1) {
                    eprintln!("  {completed}/{total} {}", text(label));
                    self.last = Instant::now();
                }
            }
            InstallerEvent::Phase { state }
                if !state.starts_with("operation:") && !state.starts_with("staging:") =>
            {
                self.bar
                    .suspend(|| eprintln!("  {}", text(state.replace('_', " "))));
            }
            InstallerEvent::Blocked { message, .. } => {
                self.bar
                    .suspend(|| eprintln!("  Blocked: {}", text(message)));
            }
            InstallerEvent::Cancelling { .. } => {
                self.bar.suspend(|| {
                    eprintln!("  Cancelling safely. Please wait for rollback to finish.")
                });
            }
            InstallerEvent::PrerequisiteDownload {
                id,
                completed,
                total,
            } => {
                self.bar.set_message(format!(
                    "Downloading {}: {completed}/{}",
                    text(id),
                    total
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "?".into())
                ));
            }
            InstallerEvent::PrerequisiteInstall { name, .. } => {
                self.bar
                    .suspend(|| eprintln!("  Installing prerequisite: {}", text(name)));
            }
            _ => {}
        }
    }
    pub fn finish(&mut self) {
        self.bar.finish_and_clear();
    }
}
