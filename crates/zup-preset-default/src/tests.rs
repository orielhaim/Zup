//! The projection, tested without a window.
//!
//! These are the rules that decide what a person is offered, and they are worth
//! checking directly: a control that appears when the host said no, or one that
//! disappears when it said yes, is a bug that only shows up as a dead button in
//! front of somebody installing software.

use zup_ui_sdk::prelude::*;
use zup_ui_sdk::zup_ui_protocol::ProgressPresentation;

use crate::view::{Page, controls as projection};

/// Every state the protocol can publish, so a page test can walk all of them.
fn every_state() -> Vec<UiState> {
    vec![
        UiState::Options,
        UiState::Maintenance,
        UiState::Running,
        UiState::WaitingForSafeCancellation,
        UiState::Succeeded,
        UiState::Failed,
        UiState::RecoveryRequired,
        UiState::ConfirmUninstall,
        UiState::Blocked {
            blockers: vec!["aurora.exe".into()],
        },
    ]
}

/// Every state the host can publish lands on a page, and every page is reachable.
///
/// Total, because a state with no page is a window that has to invent one, and an
/// invented page is a page a preset author designs against and a user never
/// meets. Shared, because two states that mean the same thing to a person are one
/// page: a cancelled-but-still-running operation is the progress page, not a page
/// of its own.
#[test]
fn every_state_lands_on_a_reachable_page() {
    let mut reached: Vec<Page> = Vec::new();
    for state in every_state() {
        let page = Page::of(&state);
        assert!(
            !page.heading().is_empty(),
            "{state:?} lands on a page with no name, which is a page nobody can read"
        );
        if !reached.contains(&page) {
            reached.push(page);
        }
    }
    assert_eq!(
        reached.len(),
        6,
        "nine states reach six pages, because running and stopping are one page \
         and failed and unreconciled are another: {reached:?}"
    );
    for page in reached {
        assert!(
            Page::step(Page::rail(true), page).is_some()
                || Page::step(Page::rail(false), page).is_some(),
            "{page:?} is reachable from a state but is on neither rail, so the step \
             indicator would not know where it is"
        );
    }
}

/// A machine being installed and one that already has it are different flows, and
/// a rail that showed one as the other would put "Uninstall" in front of somebody
/// who has nothing to uninstall.
#[test]
fn the_two_flows_have_two_rails() {
    let installing = Page::rail(true);
    let installed = Page::rail(false);
    assert_ne!(installing, installed);

    for page in [
        Page::Options,
        Page::Progress,
        Page::Done,
        Page::Confirm,
        Page::Problem,
    ] {
        assert!(
            Page::step(installing, page).is_some(),
            "an installation can reach {page:?} and its rail has to show it"
        );
    }
    assert!(
        Page::step(installing, Page::Maintenance).is_none(),
        "and a machine with nothing installed has no 'Installed' step, because inventing one \
         would be promising a state this session cannot be in"
    );
    assert!(
        Page::step(installed, Page::Options).is_none(),
        "and a maintenance session does not offer a fresh component choice as its \
         first step"
    );
}

/// The page a state maps to is where the rail points, so the rail and the page can
/// never disagree about how far along the flow somebody is.
#[test]
fn a_rail_always_points_at_the_page_that_is_showing() {
    // The states each flow can actually be in, paired with it. A maintenance
    // session is not an install surface, so it is never on the install rail, and
    // pretending otherwise is the mistake this test exists to catch.
    let install_states = [
        UiState::Options,
        UiState::Running,
        UiState::WaitingForSafeCancellation,
        UiState::Succeeded,
        UiState::ConfirmUninstall,
        UiState::Blocked {
            blockers: Vec::new(),
        },
        UiState::Failed,
        UiState::RecoveryRequired,
    ];
    let maintenance_states = [
        UiState::Maintenance,
        UiState::Running,
        UiState::WaitingForSafeCancellation,
        UiState::Succeeded,
        UiState::ConfirmUninstall,
        UiState::Blocked {
            blockers: Vec::new(),
        },
        UiState::Failed,
        UiState::RecoveryRequired,
    ];
    for (installing, states) in [
        (true, &install_states[..]),
        (false, &maintenance_states[..]),
    ] {
        let rail = Page::rail(installing);
        for state in states {
            let page = Page::of(state);
            assert!(
                Page::step(rail, page).is_some(),
                "installing={installing} state={state:?} page={page:?} is not on that flow's rail"
            );
        }
    }
    assert_eq!(
        Page::step(Page::rail(false), Page::Options),
        None,
        "and the maintenance rail has no 'Options' step, because a session that \
         already installed the application does not offer a fresh choice of what to \
         install as its first page"
    );
}

/// A page that is on the rail sits at a real position, and the first step of a
/// flow is the one a person meets first.
#[test]
fn the_first_step_of_each_flow_is_where_a_person_starts() {
    assert_eq!(Page::step(Page::rail(true), Page::Options), Some(0));
    assert_eq!(
        Page::step(Page::rail(false), Page::Maintenance),
        Some(0),
        "a maintenance session opens on what is installed, which is the question a \
         person has when they go looking for it"
    );
}

/// A snapshot of a fresh install of two components, one optional.
fn installing() -> UiSnapshot {
    UiSnapshot {
        product: ProductIdentity {
            name: "Aurora".into(),
            publisher: Some("Aurora Works".into()),
            version: "4.2.0".into(),
            description: Some("A drawing program.".into()),
        },
        surface: UiSurface::Install(InstallOptions {
            existing_version: None,
            scopes: vec![InstallScope::User, InstallScope::Machine],
            scope: InstallScope::User,
            components: vec![
                ComponentOption {
                    id: ComponentId::new("core").unwrap(),
                    name: "Aurora".into(),
                    description: None,
                    required: true,
                    selected: true,
                },
                ComponentOption {
                    id: ComponentId::new("examples").unwrap(),
                    name: "Examples".into(),
                    description: Some("Sample drawings.".into()),
                    required: false,
                    selected: false,
                },
            ],
            install_directory: Some("C:/Program Files/Aurora".into()),
            allow_directory_override: true,
        }),
        state: UiState::Options,
        progress: None,
        plan: None,
        diagnostic: None,
        update: None,
        repair_drift: Vec::new(),
    }
}

/// A snapshot of an installation that exists.
fn installed() -> UiSnapshot {
    UiSnapshot {
        surface: UiSurface::Maintenance(MaintenanceState {
            installed_version: "4.1.0".into(),
            components: vec![ComponentOption {
                id: ComponentId::new("core").unwrap(),
                name: "Aurora".into(),
                description: None,
                required: true,
                selected: true,
            }],
            updates_enabled: true,
            scope: InstallScope::Machine,
            install_directory: Some("C:/Program Files/Aurora".into()),
            health: InstallationHealth::Unknown,
        }),
        state: UiState::Maintenance,
        ..installing()
    }
}

/// The header names the application and its publisher, and never a version
/// twice.
#[test]
fn the_header_names_the_product() {
    let offered = projection(&installing());
    assert_eq!(offered.title, "Aurora");
    assert_eq!(offered.subtitle, "Aurora Works · 4.2.0");
}

/// A fresh install offers the scope choice the host offered, and only that.
#[test]
fn a_fresh_install_offers_the_scopes_the_host_offered() {
    let offered = projection(&installing());
    assert_eq!(offered.scopes.len(), 2);
    assert!(offered.scopes[0].selected);
    assert_eq!(
        offered.scopes[1].action,
        UiAction::SetScope {
            scope: InstallScope::Machine
        }
    );
}

/// The control for a component asks for the component's *opposite*, because a
/// control showing the current state is asking for a change.
#[test]
fn a_component_control_asks_for_the_opposite_of_what_it_shows() {
    let offered = projection(&installing());
    let examples = offered
        .components
        .iter()
        .find(|choice| choice.id == "examples")
        .expect("the optional component is offered");
    assert!(!examples.selected);
    assert_eq!(
        examples.action,
        UiAction::SetComponent {
            component: ComponentId::new("examples").unwrap(),
            selected: true
        }
    );
    let core = offered
        .components
        .iter()
        .find(|choice| choice.id == "core")
        .expect("the required component is offered");
    assert!(
        core.required,
        "a required component is drawn, not turned off"
    );
}

/// A package that is replacing an installation says so on the button.
#[test]
fn an_upgrade_says_it_is_an_upgrade() {
    let mut snapshot = installing();
    let UiSurface::Install(options) = &mut snapshot.surface else {
        unreachable!("a fresh install")
    };
    options.existing_version = Some("4.1.0".into());
    assert_eq!(projection(&snapshot).primary_label, "Upgrade");
}

/// The location is only editable when the application allows it, and the
/// default preset offers a preview of what a click would do.
#[test]
fn a_package_that_forbids_a_location_offers_no_location_box() {
    let mut snapshot = installing();
    let UiSurface::Install(options) = &mut snapshot.surface else {
        unreachable!("a fresh install")
    };
    options.allow_directory_override = false;
    let offered = projection(&snapshot);
    assert!(!offered.editable_directory);
    assert_eq!(
        offered.directory.as_deref(),
        Some("C:/Program Files/Aurora")
    );
    assert!(
        offered
            .secondary
            .contains(&("Show what will change".into(), UiAction::Preview))
    );
}

/// A machine that is busy offers exactly one way out, and a state that is not
/// running offers no progress at all.
#[test]
fn a_running_operation_offers_a_stop_and_reports_a_position() {
    let mut snapshot = installing();
    snapshot.state = UiState::Running;
    snapshot.progress = Some(ProgressPresentation {
        phase: OperationPhase::Files,
        completed: 3,
        total: 12,
        label: "Installing files…".into(),
    });
    let offered = projection(&snapshot);
    let status = offered
        .status
        .expect("a running operation says where it is");
    assert_eq!(status.heading, "Installing files…");
    assert_eq!(status.percent, Some(25.0));
    assert!(
        offered
            .secondary
            .contains(&("Stop".into(), UiAction::Cancel)),
        "a person has to be able to stop an operation"
    );
    assert!(
        !offered
            .secondary
            .contains(&("Show what will change".into(), UiAction::Preview)),
        "previewing mid-operation would describe a state that is about to change"
    );
}

/// After a cancel was asked for, the window says it is waiting rather than
/// offering the same stop again.
#[test]
fn a_cancellation_in_flight_is_not_offered_twice() {
    let mut snapshot = installing();
    snapshot.state = UiState::WaitingForSafeCancellation;
    let offered = projection(&snapshot);
    let status = offered
        .status
        .expect("a cancellation says what it is waiting for");
    assert!(!status.cancellable);
    assert!(
        !offered
            .secondary
            .contains(&("Stop".into(), UiAction::Cancel))
    );
}

/// Blocked means something specific is in the way, and the window shows it
/// rather than a generic failure.
#[test]
fn a_blocked_machine_shows_what_is_holding_it() {
    let mut snapshot = installing();
    snapshot.state = UiState::Blocked {
        blockers: vec!["aurora.exe (pid 4211)".into(), "aura-helper.exe".into()],
    };
    let offered = projection(&snapshot);
    let status = offered.status.expect("a blocked machine says so");
    assert!(status.detail.expect("the blockers").contains("pid 4211"));
    assert!(offered.primary == Some(UiAction::Retry) && offered.primary_label == "Try again",);
}

/// A failure is shown as a diagnosis, and a retry is offered.
#[test]
fn a_failure_shows_the_diagnosis_and_offers_a_retry() {
    let mut snapshot = installing();
    snapshot.state = UiState::Failed;
    snapshot.diagnostic = Some(DiagnosticPresentation {
        kind: DiagnosticKind::Verification,
        title: "A file did not match its hash".into(),
        meaning: "The download is not the file this release describes.".into(),
        recovery: "Check the network and try again.".into(),
        technical_details: None,
    });
    let offered = projection(&snapshot);
    assert_eq!(
        offered.status.expect("a failure is explained").heading,
        "A file did not match its hash"
    );
    assert!(offered.primary == Some(UiAction::Retry) && offered.primary_label == "Try again",);
    assert!(
        offered
            .secondary
            .contains(&("Copy diagnostics".into(), UiAction::CopyDiagnostics)),
        "a failure is when somebody needs to report it"
    );
}

/// An uninstall is asked for and confirmed separately. The window never shows
/// "Uninstall" as the primary action before the host has asked.
#[test]
fn an_uninstall_is_confirmed_before_it_is_offered() {
    let offered = projection(&installed());
    assert_eq!(offered.primary, Some(UiAction::Modify));
    assert!(
        offered
            .secondary
            .contains(&("Uninstall".into(), UiAction::RequestUninstall)),
        "asking is not doing"
    );

    let mut asking = installed();
    asking.state = UiState::ConfirmUninstall;
    let offered = projection(&asking);
    assert_eq!(offered.primary, Some(UiAction::ConfirmUninstall));
    assert_eq!(offered.primary_label, "Uninstall");
    assert!(
        offered
            .secondary
            .contains(&("Keep it".into(), UiAction::DismissUninstall))
    );
}

/// An installation whose only component is required has nothing for a repair to
/// restore, so the window does not offer one.
#[test]
fn a_repair_is_offered_only_where_something_could_have_drifted() {
    let offered = projection(&installed());
    assert!(
        !offered
            .secondary
            .contains(&("Repair".into(), UiAction::Repair)),
        "repairing a single required component changes nothing"
    );

    let mut optional = installed();
    let UiSurface::Maintenance(state) = &mut optional.surface else {
        unreachable!("an installed application")
    };
    state.components.push(ComponentOption {
        id: ComponentId::new("examples").unwrap(),
        name: "Examples".into(),
        description: None,
        required: false,
        selected: true,
    });
    assert!(
        projection(&optional)
            .secondary
            .contains(&("Repair".into(), UiAction::Repair))
    );
}

/// Updates are offered only where the application configured a channel.
#[test]
fn an_update_is_offered_only_where_a_channel_was_configured() {
    assert!(
        projection(&installed())
            .secondary
            .contains(&("Check for updates".into(), UiAction::Update))
    );

    let mut without = installed();
    let UiSurface::Maintenance(state) = &mut without.surface else {
        unreachable!("an installed application")
    };
    state.updates_enabled = false;
    assert!(
        !projection(&without)
            .secondary
            .contains(&("Check for updates".into(), UiAction::Update))
    );
}

/// Whatever the state, there is a way to close the window, and it is never the
/// same control as the one that applies something.
/// Whatever the state, the window has a way forward, and a machine that still
/// needs something from a person never answers with "Close".
#[test]
fn every_state_offers_a_way_out() {
    for state in [
        UiState::Options,
        UiState::Maintenance,
        UiState::Running,
        UiState::WaitingForSafeCancellation,
        UiState::Succeeded,
        UiState::Failed,
        UiState::RecoveryRequired,
        UiState::ConfirmUninstall,
        UiState::Blocked {
            blockers: Vec::new(),
        },
    ] {
        let mut snapshot = installing();
        snapshot.state = state.clone();
        let offered = projection(&snapshot);
        if state.is_active() {
            assert!(
                offered.primary.is_none(),
                "an operation in flight has no primary action to press: {state:?}"
            );
            continue;
        }
        assert!(
            offered.primary.is_some(),
            "every resting state offers a primary control: {state:?}"
        );
        if matches!(
            state,
            UiState::Failed | UiState::RecoveryRequired | UiState::Blocked { .. }
        ) {
            assert_ne!(
                offered.primary_label, "Close",
                "a machine that still needs something must not answer with Close: {state:?}"
            );
        }
    }
}

/// A finished operation is complete, and says so at 100%.
#[test]
fn a_succeeded_operation_is_shown_as_finished() {
    let mut snapshot = installing();
    snapshot.state = UiState::Succeeded;
    let offered = projection(&snapshot);
    let status = offered.status.expect("a finished operation says so");
    assert_eq!(status.heading, "Done");
    assert_eq!(status.percent, Some(100.0));
    assert_eq!(offered.primary, Some(UiAction::Close));
}
