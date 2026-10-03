//! The host's lifecycle state machine, driven without a window.
//!
//! Every assertion here is about what a preset would see. A host that renders
//! correctly but reports the wrong state is the failure this exists to catch,
//! and a test that needs a window cannot run in the portable matrix.

use zup_core::{ComponentId as EngineComponentId, InstallScope, Installer, SelectedScope};
use zup_exec::{InstallLedger, LifecycleAction};
use zup_preset_protocol::{
    Action, Capabilities, ComponentId, DiagnosticKind, InstallationHealth, InstallerState,
    OperationPhase, Surface, UpdateState,
};
use zup_runtime::{InstallOutcome, RuntimeEvent, RuntimeState};

use crate::{ActionRefusal, HostDecision, HostState, Selection, surface};

fn installer() -> Installer {
    Installer {
        preset: None,
        app: zup_core::App {
            id: zup_core::AppId::new("com.acme.app").expect("id"),
            name: zup_core::NonEmptyString::new("Acme").expect("name"),
            version: semver::Version::parse("1.4.0").expect("version"),
            publisher: Some(zup_core::NonEmptyString::new("Acme Inc").expect("publisher")),
            main: None,
            description: Some("The Acme application.".into()),
        },
        target: zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("target"),
        frontend: zup_core::Frontend::Gui,
        updates: None,
        install: zup_core::Install {
            scope: InstallScope::Either,
            directory: zup_core::InstallDirectory {
                user: Some(zup_core::Template::parse("${location.programs}/Acme").expect("t")),
                machine: None,
            },
            allow_directory_override: true,
        },
        prerequisites: Vec::new(),
        components: vec![
            zup_core::Component {
                id: EngineComponentId::new("core").expect("id"),
                name: zup_core::NonEmptyString::new("Core").expect("name"),
                description: None,
                required: true,
                default: true,
                requires: Vec::new(),
                group: None,
            },
            zup_core::Component {
                id: EngineComponentId::new("docs").expect("id"),
                name: zup_core::NonEmptyString::new("Documentation").expect("name"),
                description: Some("Manuals and guides.".into()),
                required: false,
                default: false,
                requires: Vec::new(),
                group: None,
            },
        ],
        component_groups: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    }
}

/// The blank plan a test fills in, so a host assertion never depends on how a
/// transaction plan is built.
fn preview(scope: SelectedScope) -> zup_presentation::PlanPreview {
    zup_presentation::PlanPreview {
        application: "com.acme.app".into(),
        version: "1.4.0".into(),
        scope,
        install_directory: String::new(),
        selected_components: Vec::new(),
        estimated_bytes: 0,
        download_bytes: 0,
        requires_authorization: false,
        groups: Vec::new(),
        requirements: Vec::new(),
    }
}

fn install_host() -> HostState {
    let installer = installer();
    let options = surface::install_options(&installer, SelectedScope::User, None, None, None);
    HostState::install(
        surface::product(&installer),
        options,
        surface::capabilities(&installer, false),
    )
}

fn maintenance_host() -> HostState {
    let mut installer = installer();
    installer.updates = Some(zup_core::UpdateConfig {
        repository: "https://updates.acme.test".into(),
        channel: "stable".into(),
        trusted_root: Vec::new(),
    });
    let mut ledger = InstallLedger::new(
        installer.app.id.clone(),
        installer.target.clone(),
        SelectedScope::Machine,
    );
    ledger.version = installer.app.version.clone();
    ledger.selected_components = vec![EngineComponentId::new("core").expect("id")];
    HostState::maintenance(
        surface::product(&installer),
        surface::maintenance_state(&installer, &ledger, SelectedScope::Machine),
        surface::capabilities(&installer, true),
    )
}

#[test]
fn a_fresh_installation_opens_on_its_choices() {
    let host = install_host();
    assert_eq!(host.snapshot().state, InstallerState::Options);
    let Surface::Install(options) = &host.snapshot().surface else {
        panic!("an install surface")
    };
    assert_eq!(
        options.scopes,
        [
            zup_preset_protocol::InstallScope::User,
            zup_preset_protocol::InstallScope::Machine
        ]
    );
    // A fresh session offers what the application declares: the required
    // component on, the optional one at its default.
    let core = options
        .components
        .iter()
        .find(|component| component.id.as_str() == "core")
        .expect("the required component");
    let docs = options
        .components
        .iter()
        .find(|component| component.id.as_str() == "docs")
        .expect("the optional component");
    assert!(core.selected && core.required);
    assert!(!docs.selected && !docs.required);
    assert!(options.allow_directory_override);
}

#[test]
fn an_existing_installation_opens_on_its_maintenance() {
    let host = maintenance_host();
    assert_eq!(host.snapshot().state, InstallerState::Maintenance);
    let Surface::Maintenance(state) = &host.snapshot().surface else {
        panic!("a maintenance surface")
    };
    assert_eq!(state.installed_version, "1.4.0");
    assert!(state.updates_enabled);
    assert_eq!(state.scope, zup_preset_protocol::InstallScope::Machine);
}

#[test]
fn an_install_runs_reports_progress_and_succeeds() {
    let mut host = install_host();
    let decision = host.accept(Action::Install);
    let HostDecision::Run {
        action,
        selection,
        cleanup_lock,
    } = decision
    else {
        panic!("an install runs a lifecycle")
    };
    assert_eq!(action, LifecycleAction::Install);
    assert_eq!(selection.scope, SelectedScope::User);
    assert!(!cleanup_lock);
    assert_eq!(host.snapshot().state, InstallerState::Running);
    assert!(host.snapshot().state.is_active());

    host.observe(&RuntimeEvent::StagingStarted { id: "stage".into() });
    assert_eq!(
        host.snapshot().progress.as_ref().expect("progress").label,
        "Preparing files…"
    );

    host.observe(&RuntimeEvent::Progress {
        completed: 40,
        total: 100,
        action: "Installing files".into(),
    });
    let progress = host.snapshot().progress.as_ref().expect("progress");
    assert_eq!(progress.phase, OperationPhase::Files);
    assert_eq!(progress.percent(), Some(40));

    host.observe(&RuntimeEvent::OperationStarted {
        id: "node.service.register".into(),
    });
    assert_eq!(
        host.snapshot().progress.as_ref().expect("progress").label,
        "Registering services…"
    );

    host.finish_with(&InstallOutcome::Committed);
    assert_eq!(host.snapshot().state, InstallerState::Succeeded);
    assert!(host.snapshot().progress.is_none());
    assert!(!host.snapshot().state.is_active());
}

/// A person who cancels is not told the operation failed. The engine stops at a
/// safe boundary and the session returns to where it started.
#[test]
fn a_cancelled_operation_returns_to_its_surface() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::Progress {
        completed: 10,
        total: 100,
        action: "Installing files".into(),
    });
    assert!(matches!(host.accept(Action::Cancel), HostDecision::Cancel));
    assert_eq!(
        host.snapshot().state,
        InstallerState::WaitingForSafeCancellation
    );

    // Progress continues to arrive while the engine walks to a safe point, and
    // must not pretend the cancellation request was withdrawn.
    host.observe(&RuntimeEvent::Progress {
        completed: 20,
        total: 100,
        action: "Installing files".into(),
    });
    assert_eq!(
        host.snapshot().state,
        InstallerState::WaitingForSafeCancellation
    );

    host.finish_with(&InstallOutcome::Cancelled);
    assert_eq!(host.snapshot().state, InstallerState::Options);
    assert!(host.snapshot().diagnostic.is_none());
}

/// The engine reports cancellation as events, and those events are not a success.
#[test]
fn a_cancelled_operation_reported_by_the_engine_returns_to_its_surface() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::StateChanged {
        state: zup_runtime::RuntimeState::Cancelled,
    });
    host.observe(&RuntimeEvent::Completed {
        outcome: "cancelled".into(),
    });
    assert_eq!(host.snapshot().state, InstallerState::Options);
    assert!(host.snapshot().diagnostic.is_none());
    assert!(!host.snapshot().state.is_active());
}

#[test]
fn a_blocked_machine_reports_the_applications_holding_it() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::ResourceBlocked {
        detail: "Editor.exe\nAgent.exe".into(),
        pids: vec![4820, 7312],
    });
    assert_eq!(
        host.snapshot().state,
        InstallerState::Blocked {
            blockers: vec!["Editor.exe".into(), "Agent.exe".into()]
        }
    );

    // The preflight's own outcome must not overwrite the actionable state with
    // a generic failure.
    host.finish_with(&InstallOutcome::Failed(
        "blocked by running applications".into(),
    ));
    assert!(matches!(
        host.snapshot().state,
        InstallerState::Blocked { .. }
    ));
    assert!(!host.snapshot().state.is_active());

    assert!(matches!(
        host.accept(Action::Retry),
        HostDecision::Run { .. }
    ));
    assert_eq!(host.snapshot().state, InstallerState::Running);
}

#[test]
fn a_recovery_failure_is_distinct_from_an_ordinary_one() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::Failed {
        kind: "recovery_required".into(),
        message: "the journal is incomplete".into(),
    });
    assert_eq!(host.snapshot().state, InstallerState::RecoveryRequired);
    let diagnostic = host.snapshot().diagnostic.as_ref().expect("a diagnostic");
    assert_eq!(diagnostic.kind, DiagnosticKind::Recovery);
    assert!(host.snapshot().progress.is_none());
}

#[test]
fn a_reboot_is_a_failure_and_not_a_success() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::Completed {
        outcome: "reboot_required".into(),
    });
    assert_eq!(host.snapshot().state, InstallerState::Failed);
}

#[test]
fn a_second_operation_is_refused_while_one_is_running() {
    let mut host = install_host();
    host.accept(Action::Install);
    assert_eq!(
        host.accept(Action::Install),
        HostDecision::Refused(ActionRefusal::Busy)
    );
    assert_eq!(
        host.accept(Action::SetScope {
            scope: zup_preset_protocol::InstallScope::Machine
        }),
        HostDecision::Refused(ActionRefusal::Busy),
        "the choices an operation is running with do not change under it"
    );
    assert_eq!(host.accept(Action::Cancel), HostDecision::Cancel);
    assert_eq!(
        host.accept(Action::Cancel),
        HostDecision::Refused(ActionRefusal::Busy)
    );
}

#[test]
fn a_retry_without_an_operation_is_refused() {
    let mut host = install_host();
    assert_eq!(
        host.accept(Action::Retry),
        HostDecision::Refused(ActionRefusal::NothingToRetry)
    );
    assert_eq!(
        host.accept(Action::Cancel),
        HostDecision::Refused(ActionRefusal::NothingToCancel)
    );
}

#[test]
fn a_required_component_cannot_be_turned_off() {
    let mut host = install_host();
    assert_eq!(
        host.accept(Action::SetComponent {
            component: ComponentId::new("core").expect("id"),
            selected: false,
        }),
        HostDecision::Refused(ActionRefusal::RequiredComponent("core".into()))
    );
    assert_eq!(
        host.accept(Action::SetComponent {
            component: ComponentId::new("absent").expect("id"),
            selected: true,
        }),
        HostDecision::Refused(ActionRefusal::UnknownComponent("absent".into()))
    );
    assert!(matches!(
        host.accept(Action::SetComponent {
            component: ComponentId::new("docs").expect("id"),
            selected: true,
        }),
        HostDecision::Plan(_)
    ));
    let Surface::Install(options) = &host.snapshot().surface else {
        panic!("an install surface")
    };
    assert!(options.components[1].selected);
    assert_eq!(
        host.accept(Action::SetComponent {
            component: ComponentId::new("docs").expect("id"),
            selected: true,
        }),
        HostDecision::Acknowledged,
        "asking for the choice already made changes nothing and plans nothing"
    );
}

/// The selection a preset changed is the selection the engine receives, with no
/// second copy of the truth to fall out of step.
#[test]
fn the_selection_the_host_reports_is_the_one_it_will_run() {
    let mut host = install_host();
    host.accept(Action::SetScope {
        scope: zup_preset_protocol::InstallScope::Machine,
    });
    host.accept(Action::SetComponent {
        component: ComponentId::new("docs").expect("id"),
        selected: false,
    });
    let HostDecision::Run { selection, .. } = host.accept(Action::Install) else {
        panic!("an install runs a lifecycle")
    };
    let expected = Selection {
        scope: SelectedScope::Machine,
        components: vec![EngineComponentId::new("core").expect("id")],
        install_directory: None,
    };
    assert_eq!(selection, expected);
}

#[test]
fn a_scope_the_application_does_not_offer_is_refused() {
    let mut host = install_host();
    assert!(matches!(
        host.accept(Action::SetScope {
            scope: zup_preset_protocol::InstallScope::Machine
        }),
        HostDecision::Plan(_)
    ));
    let mut fixed = installer();
    fixed.install.scope = InstallScope::User;
    let options = surface::install_options(&fixed, SelectedScope::User, None, None, None);
    let mut host = HostState::install(
        surface::product(&fixed),
        options,
        surface::capabilities(&fixed, false),
    );
    assert_eq!(
        host.accept(Action::SetScope {
            scope: zup_preset_protocol::InstallScope::Machine
        }),
        HostDecision::Refused(ActionRefusal::UnsupportedScope)
    );
}

#[test]
fn a_location_the_application_forbids_is_refused() {
    let mut host = install_host();
    assert!(matches!(
        host.accept(Action::SetInstallDirectory {
            directory: r"C:\Apps\Acme".into()
        }),
        HostDecision::Plan(_)
    ));
    assert_eq!(
        host.snapshot().surface.install_directory(),
        Some(r"C:\Apps\Acme")
    );
    assert!(matches!(
        host.accept(Action::ResetInstallDirectory),
        HostDecision::Plan(_)
    ));
    assert_eq!(host.snapshot().surface.install_directory(), None);

    let mut fixed = installer();
    fixed.install.allow_directory_override = false;
    let options = surface::install_options(&fixed, SelectedScope::User, None, None, None);
    let mut host = HostState::install(
        surface::product(&fixed),
        options,
        surface::capabilities(&fixed, false),
    );
    assert_eq!(
        host.accept(Action::SetInstallDirectory {
            directory: r"C:\Apps\Acme".into()
        }),
        HostDecision::Refused(ActionRefusal::DirectoryNotAllowed)
    );
}

/// A plan is kept, not requested: the session asks for one when it opens, and
/// every change of choice asks again.
#[test]
fn a_plan_answers_the_current_choices() {
    let mut host = install_host();
    assert!(host.snapshot().plan.is_computing());
    assert!(host.plan_request().is_some());
    let mut preview = preview(SelectedScope::User);
    preview.selected_components = vec![EngineComponentId::new("core").expect("id")];
    preview.install_directory = r"C:\Apps\Acme".into();
    preview.estimated_bytes = 4096;
    preview.groups = vec![zup_presentation::ChangeGroup {
        category: zup_presentation::ResourceCategory::Files,
        title: "Files".into(),
        changes: vec![zup_presentation::PlannedChange {
            category: zup_presentation::ResourceCategory::Files,
            kind: zup_presentation::ChangeKind::Create,
            label: "acme.exe".into(),
            location: Some(r"C:\Apps\Acme\acme.exe".into()),
            scope: Some(SelectedScope::User),
            requires_authorization: false,
            estimated_bytes: 4096,
            component: None,
            technical_key: Some("File { destination }".into()),
        }],
    }];
    host.set_plan(preview.clone());
    let plan = host.snapshot().plan.current().expect("a plan");
    assert_eq!(plan.estimated_bytes, 4096);
    assert_eq!(plan.scope, zup_preset_protocol::InstallScope::User);
    assert_eq!(
        plan.groups[0].changes[0].kind,
        zup_preset_protocol::ChangeKind::Create
    );

    assert!(matches!(
        host.accept(Action::SetScope {
            scope: zup_preset_protocol::InstallScope::Machine
        }),
        HostDecision::Plan(_)
    ));
    assert_eq!(
        host.snapshot()
            .plan
            .latest()
            .map(|plan| plan.estimated_bytes),
        Some(4096),
        "the last answer stays readable while the next is worked out"
    );
    host.set_plan(preview);
    assert!(
        host.snapshot().plan.is_computing(),
        "an answer for the user scope does not answer the machine scope"
    );
}

/// A plan that cannot be worked out is not a failed installation.
#[test]
fn a_plan_that_fails_leaves_the_choices_standing() {
    let mut host = install_host();
    host.plan_failed("the package could not be read".into());
    assert_eq!(host.snapshot().state, InstallerState::Options);
    assert!(matches!(
        host.snapshot().plan,
        zup_preset_protocol::PlanStatus::Failed { .. }
    ));
    assert!(matches!(
        host.accept(Action::Install),
        HostDecision::Run { .. }
    ));
}

#[test]
fn a_repair_reports_the_resources_it_left_alone() {
    let mut host = maintenance_host();
    assert!(matches!(
        host.accept(Action::Repair),
        HostDecision::Run {
            action: LifecycleAction::Repair { .. },
            ..
        }
    ));
    host.set_repair_drift(vec!["file:settings.json".into()]);
    assert_eq!(host.snapshot().repair_drift, ["file:settings.json"]);
    let Surface::Maintenance(state) = &host.snapshot().surface else {
        panic!("a maintenance surface")
    };
    assert_eq!(
        state.health,
        InstallationHealth::Drifted {
            resources: vec!["file:settings.json".into()]
        }
    );
}

#[test]
fn an_uninstall_asks_before_it_removes_anything() {
    let mut host = maintenance_host();
    assert!(matches!(
        host.accept(Action::RequestUninstall),
        HostDecision::Acknowledged
    ));
    assert_eq!(host.snapshot().state, InstallerState::ConfirmUninstall);

    // Confirming is the only thing that starts it.
    let HostDecision::Run {
        action,
        cleanup_lock,
        ..
    } = host.accept(Action::ConfirmUninstall)
    else {
        panic!("a confirmed uninstall runs")
    };
    assert_eq!(action, LifecycleAction::Uninstall);
    assert!(cleanup_lock);
    assert_eq!(host.snapshot().state, InstallerState::Running);
}

#[test]
fn a_dismissed_uninstall_leaves_the_installation_alone() {
    let mut host = maintenance_host();
    host.accept(Action::RequestUninstall);
    assert!(matches!(
        host.accept(Action::DismissUninstall),
        HostDecision::Acknowledged
    ));
    assert_eq!(host.snapshot().state, InstallerState::Maintenance);
    assert_eq!(
        host.accept(Action::ConfirmUninstall),
        HostDecision::Refused(ActionRefusal::NothingToConfirm)
    );
}

#[test]
fn a_fresh_installation_offers_no_maintenance_operations() {
    let mut host = install_host();
    for action in [
        Action::Update,
        Action::Modify,
        Action::Repair,
        Action::RequestUninstall,
    ] {
        let offered = format!("{action:?} is not offered by a fresh install");
        assert_eq!(
            host.accept(action),
            HostDecision::Refused(ActionRefusal::UnsupportedSurface),
            "{offered}"
        );
    }
}

#[test]
fn an_update_check_is_refused_where_there_is_no_channel() {
    let mut host = install_host();
    assert_eq!(
        host.accept(Action::Update),
        HostDecision::Refused(ActionRefusal::UnsupportedSurface)
    );
    let mut host = maintenance_host();
    assert_eq!(host.accept(Action::Update), HostDecision::Update);
}

#[test]
fn the_update_state_a_preset_reads_names_the_versions() {
    let mut host = maintenance_host();
    host.set_update(
        Some("stable".into()),
        UpdateState::Checking {
            detail: "Resolving…".into(),
        },
    );
    let update = host.snapshot().update.as_ref().expect("an update");
    assert_eq!(update.channel.as_deref(), Some("stable"));

    host.set_update(
        Some("stable".into()),
        UpdateState::Available {
            current: "1.4.0".into(),
            available: "1.5.0".into(),
        },
    );
    let update = host.snapshot().update.as_ref().expect("an update");
    assert_eq!(
        update.state,
        UpdateState::Available {
            current: "1.4.0".into(),
            available: "1.5.0".into()
        }
    );
}

#[test]
fn another_operation_running_is_not_reported_as_a_failure() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.finish_with(&InstallOutcome::Busy {
        operation: "installing",
    });
    assert_eq!(host.snapshot().state, InstallerState::Options);
    let diagnostic = host.snapshot().diagnostic.as_ref().expect("a diagnostic");
    assert_eq!(diagnostic.kind, DiagnosticKind::Busy);
}

#[test]
fn a_retry_repeats_the_intent_and_not_the_state_of_the_moment() {
    let mut host = install_host();
    host.accept(Action::SetScope {
        scope: zup_preset_protocol::InstallScope::Machine,
    });
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::Failed {
        kind: "transaction".into(),
        message: "the disk is full".into(),
    });
    assert_eq!(host.snapshot().state, InstallerState::Failed);

    let HostDecision::Run {
        action, selection, ..
    } = host.accept(Action::Retry)
    else {
        panic!("a retry runs the same lifecycle")
    };
    assert_eq!(action, LifecycleAction::Install);
    assert_eq!(selection.scope, SelectedScope::Machine);
    assert_eq!(host.snapshot().state, InstallerState::Running);
    assert!(host.snapshot().diagnostic.is_none());
}

#[test]
fn a_committed_operation_is_not_retried() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.finish_with(&InstallOutcome::Committed);
    assert_eq!(
        host.accept(Action::Retry),
        HostDecision::Refused(ActionRefusal::NothingToRetry)
    );
}

#[test]
fn every_engine_phase_reaches_a_renderable_state() {
    let states = [
        RuntimeState::Preparing,
        RuntimeState::CheckingPrerequisites,
        RuntimeState::InstallingPrerequisites,
        RuntimeState::WaitingForAuthorization,
        RuntimeState::ConnectingWorker,
        RuntimeState::Executing,
        RuntimeState::RollingBack,
    ];
    for state in states {
        let mut host = install_host();
        host.accept(Action::Install);
        host.observe(&RuntimeEvent::StateChanged { state });
        assert_eq!(
            host.snapshot().state,
            InstallerState::Running,
            "{state:?} left the run without a running state"
        );
        assert!(
            host.snapshot().progress.is_some(),
            "{state:?} left the run with nothing to show"
        );
    }
}

#[test]
fn a_rolling_back_run_reports_what_it_is_restoring() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::RollingBack);
    assert_eq!(
        host.snapshot().progress.as_ref().expect("progress").label,
        "Restoring the previous state…"
    );
    assert_eq!(host.snapshot().state, InstallerState::Running);
}

#[test]
fn a_log_is_revealed_only_once_the_engine_has_named_one() {
    let mut host = install_host();
    assert_eq!(
        host.accept(Action::OpenLog),
        HostDecision::Refused(ActionRefusal::NoLog)
    );
    host.observe(&RuntimeEvent::LogPath {
        path: r"C:\Temp\zup.log".into(),
    });
    assert_eq!(host.accept(Action::OpenLog), HostDecision::OpenLog);
}

/// A preset is refused what this application cannot do, before it is launched.
#[test]
fn capabilities_describe_the_package_rather_than_the_build() {
    let installer = installer();
    let fresh = surface::capabilities(&installer, false);
    assert!(fresh.contains(zup_preset_protocol::Capability::Components));
    assert!(fresh.contains(zup_preset_protocol::Capability::PlanPreview));
    assert!(fresh.contains(zup_preset_protocol::Capability::InstallDirectory));
    assert!(!fresh.contains(zup_preset_protocol::Capability::Maintenance));
    assert!(!fresh.contains(zup_preset_protocol::Capability::Updates));

    let mut configured = installer.clone();
    configured.updates = Some(zup_core::UpdateConfig {
        repository: "https://updates.acme.test".into(),
        channel: "stable".into(),
        trusted_root: Vec::new(),
    });
    let maintaining = surface::capabilities(&configured, true);
    assert!(maintaining.contains(zup_preset_protocol::Capability::Updates));
    assert!(maintaining.contains(zup_preset_protocol::Capability::Maintenance));

    let mut plain = installer;
    plain.components.clear();
    assert!(
        !surface::capabilities(&plain, false).contains(zup_preset_protocol::Capability::Components)
    );
}

#[test]
fn a_host_without_plans_says_so_rather_than_simulating_one() {
    let installer = installer();
    let options = surface::install_options(&installer, SelectedScope::User, None, None, None);
    let capabilities = Capabilities::default();
    let mut host = HostState::install(surface::product(&installer), options, capabilities);
    assert_eq!(
        host.snapshot().plan,
        zup_preset_protocol::PlanStatus::Unsupported
    );
    assert_eq!(host.plan_request(), None);
    assert_eq!(
        host.accept(Action::SetComponent {
            component: ComponentId::new("docs").expect("id"),
            selected: false,
        }),
        HostDecision::Acknowledged
    );
}

/// The location a plan resolves is where the default goes, not a choice the
/// person made: an install that never chose a location is not run with one.
#[test]
fn a_resolved_location_is_not_a_chosen_one() {
    let mut host = install_host();
    let mut preview = preview(SelectedScope::User);
    preview.install_directory = r"C:\Apps\Acme".into();
    host.set_plan(preview);
    let HostDecision::Run { selection, .. } = host.accept(Action::Install) else {
        panic!("an install runs a lifecycle")
    };
    assert_eq!(selection.install_directory, None);
}

/// Once an installation commits, the host offers to start it through the
/// launcher the application declared, and only then.
#[test]
fn a_committed_install_offers_its_launcher() {
    let installer = installer();
    let options = surface::install_options(&installer, SelectedScope::User, None, None, None);
    let capabilities =
        surface::capabilities(&installer, false).with(zup_preset_protocol::Capability::Launch);
    let mut host = HostState::install(surface::product(&installer), options, capabilities)
        .with_launchers(vec![crate::Launchable {
            target: zup_preset_protocol::LaunchTarget {
                name: "Acme".into(),
            },
            component: None,
        }]);
    assert_eq!(
        host.accept(Action::Launch),
        HostDecision::Refused(ActionRefusal::NothingToLaunch)
    );
    host.accept(Action::Install);
    assert_eq!(
        host.snapshot().operation,
        Some(zup_preset_protocol::OperationKind::Install)
    );
    host.finish_with(&InstallOutcome::Committed);
    assert_eq!(host.snapshot().state, InstallerState::Succeeded);
    assert!(matches!(
        host.accept(Action::Launch),
        HostDecision::Launch(target) if target.name == "Acme"
    ));
}

#[test]
fn the_maintenance_surface_offers_no_scope_or_location_choices() {
    let mut host = maintenance_host();
    assert_eq!(host.snapshot().surface.scopes().len(), 1);
    assert!(!host.snapshot().surface.allows_directory_override());
    assert_eq!(
        host.accept(Action::SetInstallDirectory {
            directory: r"C:\Apps\Acme".into()
        }),
        HostDecision::Refused(ActionRefusal::DirectoryNotAllowed)
    );
}

#[test]
fn a_prerequisite_is_presented_as_a_requirement_not_as_a_failure() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::PrerequisiteCheck {
        id: "runtime-library".into(),
        name: "runtime library".into(),
        satisfied: false,
        version: None,
    });
    assert_eq!(host.snapshot().state, InstallerState::Running);
    assert_eq!(
        host.snapshot().progress.as_ref().expect("progress").label,
        "runtime library is needed"
    );
}

/// A fresh session over an existing installation is an upgrade, and it says so
/// in the choices it offers rather than presenting itself as a first install.
#[test]
fn a_fresh_session_over_an_existing_installation_names_what_it_replaces() {
    let installer = installer();
    let mut ledger = InstallLedger::new(
        installer.app.id.clone(),
        installer.target.clone(),
        SelectedScope::User,
    );
    ledger.version = semver::Version::parse("1.0.0").expect("version");
    let options = surface::install_options(
        &installer,
        SelectedScope::User,
        Some(ledger.version.to_string()),
        None,
        Some(&ledger),
    );
    assert_eq!(options.existing_version.as_deref(), Some("1.0.0"));
    // The ledger's selection wins over the application's defaults: the machine
    // already has an answer, and asking again would offer to remove components
    // the person did not choose to remove.
    let docs = options
        .components
        .iter()
        .find(|component| component.id.as_str() == "docs")
        .expect("the optional component");
    assert!(!docs.selected);
}

#[test]
fn a_cancelled_prerequisite_download_is_still_countable_progress() {
    let mut host = install_host();
    host.accept(Action::Install);
    host.observe(&RuntimeEvent::PrerequisiteDownload {
        id: "runtime-library".into(),
        completed: 512,
        total: Some(1024),
    });
    let progress = host.snapshot().progress.as_ref().expect("progress");
    assert_eq!(progress.phase, OperationPhase::Download);
    assert_eq!(progress.percent(), Some(50));
}
