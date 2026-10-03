//! Named machines, each in a state a real installation can reach.
//!
//! A preset is judged on the states people actually meet, and most of them are
//! several steps away from a fresh window: an upgrade that finished, a repair
//! that left files alone, an uninstall waiting to be confirmed. Each entry here
//! reaches its state by the same actions and engine events a real session
//! would pass through, so a state that renders here is one a real session can
//! render.

use zup_presentation::RequirementStatus;
use zup_runtime::{InstallOutcome, RuntimeEvent, RuntimeState};
use zup_preset_protocol::{
    ComponentGroupOption, ComponentId, ComponentOption, ComponentProminence, InstallScope,
    InstallationHealth, ProductIdentity, SelectionRequirement, Action, UpdateState,
};

use crate::machine::{Footprint, Machine, Scenario, Surface};

/// One named state.
pub struct Entry {
    pub name: &'static str,
    pub about: &'static str,
    build: fn() -> Machine,
}

impl Entry {
    pub fn machine(&self) -> Machine {
        (self.build)()
    }
}

/// Every named state, in the order a person would walk through them.
pub const ENTRIES: &[Entry] = &[
    entry("simple", "a fresh install with nothing to choose", simple),
    entry(
        "components",
        "a fresh install with optional components",
        components,
    ),
    entry("primary", "a small primary component group", primary_small),
    entry(
        "many-primary",
        "a large primary group, summarized",
        many_primary,
    ),
    entry(
        "workloads",
        "a primary group and a secondary group",
        workloads,
    ),
    entry(
        "secondary-groups",
        "two secondary component groups",
        secondary_groups,
    ),
    entry(
        "explicit",
        "a primary group that must be chosen",
        explicit_choice,
    ),
    entry(
        "machine-only",
        "an application that installs for everyone",
        machine_only,
    ),
    entry(
        "machine-scope",
        "the everyone scope chosen, so approval is needed",
        machine_scope,
    ),
    entry(
        "fixed-location",
        "an application that does not allow choosing a location",
        fixed_location,
    ),
    entry(
        "custom-location",
        "a location the person chose",
        custom_location,
    ),
    entry(
        "long",
        "very long names, publisher, path and descriptions",
        long,
    ),
    entry(
        "many-components",
        "a large set of components",
        many_components,
    ),
    entry(
        "big-plan",
        "a plan that touches every kind of resource",
        big_plan,
    ),
    entry(
        "plan-failed",
        "a plan the host could not work out",
        plan_failed,
    ),
    entry("upgrade", "a newer version over an installed one", upgrade),
    entry(
        "progress",
        "an install that knows how far it has got",
        progress,
    ),
    entry(
        "progress-unknown",
        "an install that cannot count its work yet",
        progress_unknown,
    ),
    entry(
        "downloading",
        "a required component being downloaded",
        downloading,
    ),
    entry("stopping", "a cancel waiting for a safe point", stopping),
    entry("installed", "a fresh install that finished", installed),
    entry("upgraded", "an upgrade that finished", upgraded),
    entry("maintenance", "a healthy installation", maintenance),
    entry(
        "drifted",
        "an installation whose files were changed",
        drifted,
    ),
    entry(
        "update-available",
        "a newer version on the update channel",
        update_available,
    ),
    entry(
        "up-to-date",
        "an update check that found nothing newer",
        up_to_date,
    ),
    entry(
        "update-checking",
        "an update check in flight",
        update_checking,
    ),
    entry(
        "update-failed",
        "an update check that failed",
        update_failed,
    ),
    entry("modify", "changing which components are installed", modify),
    entry("repairing", "a repair in progress", repairing),
    entry("repaired", "a repair that finished", repaired),
    entry(
        "repaired-drift",
        "a repair that left changed files alone",
        repaired_drift,
    ),
    entry(
        "uninstall-confirm",
        "an uninstall waiting to be confirmed",
        uninstall_confirm,
    ),
    entry("uninstalling", "an uninstall in progress", uninstalling),
    entry("uninstalled", "an uninstall that finished", uninstalled),
    entry("blocked", "applications holding files open", blocked),
    entry("failed", "a failure that can be retried", failed),
    entry(
        "permission",
        "a failure for want of administrator approval",
        permission,
    ),
    entry(
        "verification",
        "a download that did not verify",
        verification,
    ),
    entry(
        "recovery",
        "a transaction that has to be reconciled first",
        recovery,
    ),
];

const fn entry(name: &'static str, about: &'static str, build: fn() -> Machine) -> Entry {
    Entry { name, about, build }
}

/// The named state, if there is one by that name.
pub fn named(name: &str) -> Option<Machine> {
    ENTRIES
        .iter()
        .find(|entry| entry.name == name)
        .map(Entry::machine)
}

fn component(
    id: &str,
    name: &str,
    description: &str,
    required: bool,
    selected: bool,
) -> ComponentOption {
    ComponentOption {
        id: ComponentId::new(id).expect("a component id is never empty"),
        name: name.to_owned(),
        description: (!description.is_empty()).then(|| description.to_owned()),
        required,
        selected,
        installed: false,
    }
}

fn demo() -> Scenario {
    Scenario {
        product: ProductIdentity {
            name: "Demo App".into(),
            publisher: Some("Zup Labs".into()),
            version: "1.0.0".into(),
            description: Some("A small app for taking notes.".into()),
        },
        user_directory: r"C:\Users\you\AppData\Local\Programs\Demo App".into(),
        machine_directory: r"C:\Program Files\Demo App".into(),
        installed_version: "0.9.2".into(),
        launcher: Some("Demo App".into()),
        ..Scenario::default()
    }
}

fn with_components() -> Scenario {
    Scenario {
        components: vec![
            component("app", "Demo App", "The application itself.", true, true),
            component(
                "cli",
                "Command-line tool",
                "Use Demo App from a terminal. Adds demo to your PATH.",
                false,
                true,
            ),
            component(
                "samples",
                "Sample notebooks",
                "A few notebooks to explore before you write your own.",
                false,
                false,
            ),
        ],
        footprint: Footprint {
            path: true,
            ..Footprint::default()
        },
        ..demo()
    }
}

fn open(scenario: Scenario) -> Machine {
    Machine::new(scenario)
}

fn maintained(scenario: Scenario) -> Machine {
    open(Scenario {
        surface: Surface::Maintenance,
        ..scenario
    })
}

/// Start an operation and report the events every run begins with.
fn begin(machine: &mut Machine, action: Action) {
    machine.act(action);
    machine.observe(&RuntimeEvent::StateChanged {
        state: RuntimeState::Preparing,
    });
}

fn files(machine: &mut Machine, completed: u64, total: u64) {
    machine.observe(&RuntimeEvent::StagingStarted {
        id: "application".into(),
    });
    machine.observe(&RuntimeEvent::Progress {
        completed,
        total,
        action: "Writing application files".into(),
    });
}

fn simple() -> Machine {
    open(Scenario {
        scopes: vec![InstallScope::User],
        ..demo()
    })
}

fn components() -> Machine {
    open(with_components())
}

fn declare(
    id: &str,
    label: &str,
    prominence: ComponentProminence,
    selection: SelectionRequirement,
    components: &[ComponentOption],
) -> ComponentGroupOption {
    ComponentGroupOption {
        id: id.into(),
        label: Some(label.into()),
        description: None,
        prominence,
        selection,
        components: components
            .iter()
            .map(|component| component.id.clone())
            .collect(),
    }
}

fn primary_small() -> Machine {
    let components = vec![
        component("app", "Demo App", "The application itself.", true, true),
        component(
            "cli",
            "CLI tools",
            "Commands for the terminal.",
            false,
            true,
        ),
        component(
            "ide",
            "IDE integration",
            "Open projects from the editor.",
            false,
            false,
        ),
    ];
    open(Scenario {
        components: components.clone(),
        groups: vec![declare(
            "main",
            "Choose what to install",
            ComponentProminence::Primary,
            SelectionRequirement::Defaulted,
            &components,
        )],
        scopes: vec![InstallScope::User],
        ..demo()
    })
}

fn many_primary() -> Machine {
    let mut machine = many_components();
    let components = machine.scenario().components.clone();
    machine.reopen(&Scenario {
        groups: vec![declare(
            "components",
            "Components",
            ComponentProminence::Primary,
            SelectionRequirement::Defaulted,
            &components,
        )],
        ..machine.scenario().clone()
    });
    machine
}

fn workloads() -> Machine {
    let primary = vec![
        component("web", "Web development", "", false, true),
        component("desktop", "Desktop development", "", false, false),
        component("game", "Game development", "", false, false),
    ];
    let secondary = vec![
        component("docs", "Documentation", "", false, true),
        component("samples", "Sample data", "", false, true),
        component("symbols", "Debug symbols", "", false, false),
    ];
    let mut components = primary.clone();
    components.extend(secondary.iter().cloned());
    open(Scenario {
        components,
        groups: vec![
            declare(
                "work",
                "Workloads",
                ComponentProminence::Primary,
                SelectionRequirement::Defaulted,
                &primary,
            ),
            declare(
                "extras",
                "Optional tools",
                ComponentProminence::Secondary,
                SelectionRequirement::Defaulted,
                &secondary,
            ),
        ],
        ..demo()
    })
}

fn secondary_groups() -> Machine {
    let docs = vec![
        component("guide", "User guide", "", false, true),
        component("api", "API reference", "", false, false),
    ];
    let lang = vec![
        component("en", "English dictionary", "", false, true),
        component("fr", "French dictionary", "", false, false),
        component("de", "German dictionary", "", false, false),
    ];
    let mut components = docs.clone();
    components.extend(lang.iter().cloned());
    open(Scenario {
        components,
        groups: vec![
            declare(
                "docs",
                "Documentation",
                ComponentProminence::Secondary,
                SelectionRequirement::Defaulted,
                &docs,
            ),
            declare(
                "lang",
                "Dictionaries",
                ComponentProminence::Secondary,
                SelectionRequirement::Defaulted,
                &lang,
            ),
        ],
        ..demo()
    })
}

fn explicit_choice() -> Machine {
    let components = vec![
        component(
            "web",
            "Web development",
            "Sites and services.",
            false,
            false,
        ),
        component(
            "desktop",
            "Desktop development",
            "Native applications.",
            false,
            false,
        ),
        component(
            "game",
            "Game development",
            "Real-time projects.",
            false,
            false,
        ),
    ];
    open(Scenario {
        components: components.clone(),
        groups: vec![declare(
            "work",
            "Workloads",
            ComponentProminence::Primary,
            SelectionRequirement::Explicit,
            &components,
        )],
        scopes: vec![InstallScope::User],
        ..demo()
    })
}

fn machine_only() -> Machine {
    open(Scenario {
        scopes: vec![InstallScope::Machine],
        scope: InstallScope::Machine,
        ..with_components()
    })
}

fn machine_scope() -> Machine {
    let mut machine = open(with_components());
    machine.act(Action::SetScope {
        scope: InstallScope::Machine,
    });
    machine
}

fn fixed_location() -> Machine {
    open(Scenario {
        allow_directory_override: false,
        ..with_components()
    })
}

fn custom_location() -> Machine {
    let mut machine = open(with_components());
    machine.act(Action::SetInstallDirectory {
        directory: r"D:\Apps\Demo App".into(),
    });
    machine
}

fn long() -> Machine {
    open(Scenario {
        product: ProductIdentity {
            name: "Contoso Professional Studio Workstation Edition for Enterprise Teams".into(),
            publisher: Some(
                "Contoso International Software Development and Distribution Corporation Ltd."
                    .into(),
            ),
            version: "2026.10.14-preview.3+build.48213".into(),
            description: Some(
                "A complete environment for designing, building, testing and shipping large \
                 collaborative projects across distributed teams, with offline support."
                    .into(),
            ),
        },
        user_directory: r"C:\Users\christopher.alexander-montgomery\AppData\Local\Programs\Contoso International\Contoso Professional Studio Workstation Edition for Enterprise Teams".into(),
        machine_directory: r"C:\Program Files\Contoso International\Contoso Professional Studio Workstation Edition for Enterprise Teams".into(),
        components: vec![
            component("core", "Contoso Professional Studio Workstation core runtime and shared libraries", "Everything the studio needs to start, including the shared rendering engine, the project database, and the background indexing service that keeps search fast.", true, true),
            component("toolchains", "Cross-platform toolchains for Windows, Linux, macOS, Android and embedded targets", "Compilers, linkers, debuggers and profilers for every supported platform. Large; choose only the platforms you build for if space is tight.", false, true),
            component("offline-docs", "Offline documentation", "", false, false),
        ],
        launcher: Some("Contoso Professional Studio".into()),
        ..demo()
    })
}

fn many_components() -> Machine {
    let names = [
        ("app", "Demo App", "The application itself.", true, true),
        (
            "cli",
            "Command-line tool",
            "Use Demo App from a terminal.",
            false,
            true,
        ),
        (
            "shell",
            "Explorer integration",
            "Open folders in Demo App from the right-click menu.",
            false,
            true,
        ),
        (
            "samples",
            "Sample notebooks",
            "Notebooks to explore before you write your own.",
            false,
            false,
        ),
        ("spell-en", "English dictionary", "", false, true),
        ("spell-fr", "French dictionary", "", false, false),
        ("spell-de", "German dictionary", "", false, false),
        ("spell-es", "Spanish dictionary", "", false, false),
        (
            "themes",
            "Extra themes",
            "Twelve additional colour themes.",
            false,
            false,
        ),
        (
            "fonts",
            "Bundled fonts",
            "Fonts used by the built-in templates.",
            false,
            true,
        ),
        (
            "sync",
            "Sync service",
            "Keeps notebooks in step across your devices. Runs in the background.",
            false,
            false,
        ),
        (
            "plugins",
            "Plugin SDK",
            "Headers and samples for writing your own plugins.",
            false,
            false,
        ),
        ("pdf", "PDF export", "", false, true),
        (
            "telemetry",
            "Crash reporter",
            "Sends crash reports so problems can be fixed sooner.",
            false,
            false,
        ),
    ];
    open(Scenario {
        components: names
            .iter()
            .map(|(id, name, about, required, selected)| {
                component(id, name, about, *required, *selected)
            })
            .collect(),
        ..demo()
    })
}

fn big_plan() -> Machine {
    open(Scenario {
        footprint: Footprint {
            application_bytes: 214 * 1024 * 1024,
            download_bytes: 38 * 1024 * 1024,
            files: 48,
            shortcuts: vec!["Demo App".into(), "Demo App (Safe Mode)".into()],
            path: true,
            services: vec!["Demo Sync Service".into()],
            protocols: vec!["demo".into()],
            file_associations: vec![".demo".into(), ".demonb".into()],
            requirements: vec![
                (
                    "Microsoft Visual C++ Redistributable".into(),
                    RequirementStatus::Missing,
                    25 * 1024 * 1024,
                ),
                (
                    "Microsoft Edge WebView2 Runtime".into(),
                    RequirementStatus::Satisfied,
                    0,
                ),
            ],
            ..Footprint::default()
        },
        ..with_components()
    })
}

fn plan_failed() -> Machine {
    open(Scenario {
        footprint: Footprint {
            plan_failure: Some("the package's file table could not be read".into()),
            ..Footprint::default()
        },
        ..with_components()
    })
}

fn upgrade() -> Machine {
    open(Scenario {
        existing_version: Some("0.9.2".into()),
        ..with_components()
    })
}

fn progress() -> Machine {
    let mut machine = components();
    begin(&mut machine, Action::Install);
    files(&mut machine, 41 * 1024 * 1024, 96 * 1024 * 1024);
    machine
}

fn progress_unknown() -> Machine {
    let mut machine = components();
    begin(&mut machine, Action::Install);
    machine.observe(&RuntimeEvent::PreflightStarted);
    machine
}

fn downloading() -> Machine {
    let mut machine = big_plan();
    begin(&mut machine, Action::Install);
    machine.observe(&RuntimeEvent::PrerequisiteDownload {
        id: "vcredist".into(),
        completed: 9 * 1024 * 1024,
        total: Some(25 * 1024 * 1024),
    });
    machine
}

fn stopping() -> Machine {
    let mut machine = progress();
    machine.act(Action::Cancel);
    machine
}

fn installed() -> Machine {
    let mut machine = progress();
    machine.finish_with(&InstallOutcome::Committed);
    machine
}

fn upgraded() -> Machine {
    let mut machine = upgrade();
    begin(&mut machine, Action::Install);
    machine.finish_with(&InstallOutcome::Committed);
    machine
}

fn maintenance() -> Machine {
    maintained(with_components())
}

fn drifted() -> Machine {
    maintained(Scenario {
        health: InstallationHealth::Drifted {
            resources: vec![
                r"C:\Users\you\AppData\Local\Programs\Demo App\demo-core.dll".into(),
                "Start menu shortcut \"Demo App\"".into(),
            ],
        },
        ..with_components()
    })
}

fn update_available() -> Machine {
    let mut machine = maintenance();
    machine.set_update(UpdateState::Available {
        current: "0.9.2".into(),
        available: "1.0.0".into(),
    });
    machine
}

fn up_to_date() -> Machine {
    let mut machine = maintenance();
    machine.set_update(UpdateState::UpToDate {
        current: "0.9.2".into(),
    });
    machine
}

fn update_checking() -> Machine {
    let mut machine = maintenance();
    machine.set_update(UpdateState::Checking {
        detail: "Contacting the update server".into(),
    });
    machine
}

fn update_failed() -> Machine {
    let mut machine = maintenance();
    machine.set_update(UpdateState::Failed {
        message: "The update server could not be reached.".into(),
    });
    machine
}

fn modify() -> Machine {
    let mut machine = maintenance();
    machine.act(Action::SetComponent {
        component: ComponentId::new("samples").expect("id"),
        selected: true,
    });
    machine.act(Action::SetComponent {
        component: ComponentId::new("cli").expect("id"),
        selected: false,
    });
    machine
}

fn repairing() -> Machine {
    let mut machine = drifted();
    begin(&mut machine, Action::Repair);
    files(&mut machine, 12, 30);
    machine
}

fn repaired() -> Machine {
    let mut machine = maintenance();
    begin(&mut machine, Action::Repair);
    machine.finish_with(&InstallOutcome::Committed);
    machine
}

fn repaired_drift() -> Machine {
    let mut machine = drifted();
    begin(&mut machine, Action::Repair);
    machine.finish_with(&InstallOutcome::Committed);
    machine.set_drift(vec![
        r"C:\Users\you\AppData\Local\Programs\Demo App\settings.json".into(),
    ]);
    machine
}

fn uninstall_confirm() -> Machine {
    let mut machine = maintenance();
    machine.act(Action::RequestUninstall);
    machine
}

fn uninstalling() -> Machine {
    let mut machine = uninstall_confirm();
    begin(&mut machine, Action::ConfirmUninstall);
    machine.observe(&RuntimeEvent::OperationStarted {
        id: "remove-files".into(),
    });
    machine
}

fn uninstalled() -> Machine {
    let mut machine = uninstall_confirm();
    begin(&mut machine, Action::ConfirmUninstall);
    machine.finish_with(&InstallOutcome::Committed);
    machine
}

fn blocked() -> Machine {
    let mut machine = progress();
    machine.observe(&RuntimeEvent::ResourceBlocked {
        detail: "Demo App (demo.exe)\nDemo App Helper (demo-helper.exe)".into(),
        pids: vec![4211, 4388],
    });
    machine.finish_with(&InstallOutcome::Failed(
        "blocked by running applications".into(),
    ));
    machine
}

fn failed() -> Machine {
    let mut machine = progress();
    machine.finish_with(&InstallOutcome::Failed(
        "staging failed: the payload stream ended early (os error 232)".into(),
    ));
    machine
}

fn permission() -> Machine {
    let mut machine = machine_scope();
    begin(&mut machine, Action::Install);
    machine.finish_with(&InstallOutcome::Failed(
        "elevation was declined: access is denied (os error 5)".into(),
    ));
    machine
}

fn verification() -> Machine {
    let mut machine = big_plan();
    begin(&mut machine, Action::Install);
    machine.finish_with(&InstallOutcome::Failed(
        "vcredist: digest mismatch: expected sha256:9f2c…41d0, found sha256:03ab…77e1".into(),
    ));
    machine
}

fn recovery() -> Machine {
    let mut machine = progress();
    machine.finish_with(&InstallOutcome::RecoveryRequired);
    machine
}
