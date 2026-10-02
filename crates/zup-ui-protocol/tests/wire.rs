//! What a frame has to satisfy before either peer acts on it.
//!
//! The wire rules are the only thing standing between a native executable and
//! the engine driving it, so each one is tested by breaking it rather than by
//! asserting the happy path alone.

use zup_ui_protocol::{
    ComponentId, ComponentOption, DiagnosticKind, DiagnosticPresentation, HostHello,
    InstallOptions, InstallScope, MaintenanceState, OperationPhase, PlanStatus, ProductIdentity,
    ProgressPresentation, UiAction, UiCapabilities, UiCapability, UiEnvelope, UiHello, UiMessage,
    UiSessionId, UiSnapshot, UiState, UiSurface, UiWireError, decode, encode, negotiate,
};

fn session() -> UiSessionId {
    UiSessionId::new_v7()
}

fn product() -> ProductIdentity {
    ProductIdentity {
        name: "Acme".into(),
        publisher: Some("Acme Inc".into()),
        version: "1.4.0".into(),
        description: None,
    }
}

fn snapshot() -> UiSnapshot {
    UiSnapshot {
        product: product(),
        surface: UiSurface::Install(InstallOptions {
            existing_version: None,
            scopes: vec![InstallScope::User, InstallScope::Machine],
            scope: InstallScope::User,
            components: vec![ComponentOption {
                id: ComponentId::new("core").expect("id"),
                name: "Core".into(),
                description: None,
                required: true,
                selected: true,
                installed: false,
            }],
            groups: Vec::new(),
            install_directory: None,
            allow_directory_override: true,
        }),
        state: UiState::Options,
        operation: None,
        progress: None,
        plan: PlanStatus::Unsupported,
        diagnostic: None,
        update: None,
        repair_drift: Vec::new(),
        launch: None,
    }
}

fn envelope(message: UiMessage) -> UiEnvelope {
    UiEnvelope {
        version: zup_ui_protocol::UI_PROTOCOL_VERSION,
        session: session(),
        sequence: 1,
        message,
    }
}

#[test]
fn a_frame_survives_the_round_trip() {
    let original = envelope(UiMessage::Snapshot(Box::new(snapshot())));
    let bytes = encode(&original).expect("encoded");
    assert_eq!(decode(&bytes).expect("decoded"), original);
}

#[test]
fn a_frame_from_a_different_protocol_is_refused() {
    let mut frame = envelope(UiMessage::Action(UiAction::Install));
    frame.version = zup_ui_protocol::UI_PROTOCOL_VERSION + 1;
    let bytes = encode(&frame).expect("encoded");
    assert_eq!(
        decode(&bytes).expect_err("a newer protocol is not readable"),
        UiWireError::VersionMismatch {
            expected: zup_ui_protocol::UI_PROTOCOL_VERSION,
            found: zup_ui_protocol::UI_PROTOCOL_VERSION + 1,
        }
    );
}

#[test]
fn bytes_that_are_not_a_frame_are_refused() {
    assert!(matches!(
        decode(b"not json at all"),
        Err(UiWireError::Malformed(_))
    ));
    assert!(matches!(
        decode(br#"{"version":1,"session":"nope"}"#),
        Err(UiWireError::Malformed(_))
    ));
}

/// A frame longer than the limit is refused on its declared length, before a
/// peer allocates a buffer for it.
#[test]
fn an_oversized_frame_is_refused() {
    let bytes = vec![b'x'; zup_ui_protocol::MAX_FRAME_BYTES + 1];
    assert_eq!(
        decode(&bytes).expect_err("refused"),
        UiWireError::FrameTooLarge {
            max: zup_ui_protocol::MAX_FRAME_BYTES,
        }
    );

    let huge = UiSnapshot {
        repair_drift: vec!["x".repeat(zup_ui_protocol::MAX_FRAME_BYTES)],
        ..snapshot()
    };
    assert!(encode(&envelope(UiMessage::Snapshot(Box::new(huge)))).is_err());
}

/// An action is a closed vocabulary. A peer that sends a method name and an
/// argument object is not speaking this protocol.
#[test]
fn an_action_that_is_not_in_the_vocabulary_is_refused() {
    let bytes = br#"{"version":1,"session":"0192f0f0-0000-7000-8000-000000000000","sequence":1,"message":{"type":"action","action":"run_method","method":"install","arguments":{}}}"#;
    assert!(matches!(decode(bytes), Err(UiWireError::Malformed(_))));
}

#[test]
fn sequences_move_forward_and_never_repeat() {
    let mut tracker = zup_ui_protocol::SequenceTracker::new();
    tracker.accept(1).expect("first");
    tracker.accept(9).expect("later");
    assert_eq!(
        tracker.accept(9).expect_err("a repeat is a resend"),
        UiWireError::DuplicateSequence { sequence: 9 }
    );
    assert_eq!(
        tracker
            .accept(4)
            .expect_err("a lower value is a reordering"),
        UiWireError::SequenceRegression {
            previous: 9,
            next: 4
        }
    );
}

#[test]
fn a_preset_is_refused_when_the_host_cannot_provide_what_it_requires() {
    let provided = UiCapabilities::new([UiCapability::Components, UiCapability::PlanPreview]);
    let required = UiCapabilities::new([
        UiCapability::Components,
        UiCapability::Maintenance,
        UiCapability::Updates,
    ]);
    let error = negotiate(&provided, &required).expect_err("refused");
    assert_eq!(
        error,
        UiWireError::MissingCapability("maintenance, updates".into())
    );
    assert!(negotiate(&provided, &UiCapabilities::new([UiCapability::Components])).is_ok());
}

#[test]
fn capability_names_are_a_closed_vocabulary() {
    assert!(UiCapability::parse("maintenance").is_some());
    assert!(UiCapability::parse("telemetry").is_none());
    let capabilities: UiCapabilities = [UiCapability::Updates, UiCapability::Components]
        .into_iter()
        .collect();
    assert_eq!(capabilities.names(), ["components", "updates"]);
}

#[test]
fn the_handshake_names_the_protocol_and_the_session_on_both_sides() {
    let id = session();
    let hello = UiHello {
        protocol_version: zup_ui_protocol::UI_PROTOCOL_VERSION,
        session: id,
        preset: "aurora".into(),
        preset_version: "2.1.0".into(),
        required_capabilities: [UiCapability::PlanPreview].into_iter().collect(),
    };
    let host = HostHello::new(
        id,
        [UiCapability::PlanPreview, UiCapability::Diagnostics]
            .into_iter()
            .collect(),
        product(),
        "0.8.4",
    );
    assert_eq!(host.session, hello.session);
    assert!(negotiate(&host.capabilities, &hello.required_capabilities).is_ok());
}

/// Two sessions may legitimately be running at once on one machine, so the id
/// is what tells a frame apart from a straggler of a session that already ended.
#[test]
fn a_frame_from_another_session_is_recognizable() {
    let first = envelope(UiMessage::Action(UiAction::Install));
    let second = UiEnvelope {
        session: UiSessionId::new_v7(),
        ..first.clone()
    };
    assert_ne!(first.session, second.session);
}

#[test]
fn every_lifecycle_state_and_action_survives_the_wire() {
    let states = [
        UiState::Options,
        UiState::Maintenance,
        UiState::Running,
        UiState::WaitingForSafeCancellation,
        UiState::Blocked {
            blockers: vec!["Acme.exe".into()],
        },
        UiState::ConfirmUninstall,
        UiState::Succeeded,
        UiState::Failed,
        UiState::RecoveryRequired,
    ];
    for state in states {
        let bytes = serde_json::to_string(&state).expect("encoded");
        assert_eq!(
            bytes,
            serde_json::to_string(&decode_state(&bytes)).expect("decoded")
        );
    }

    let actions = [
        UiAction::SetScope {
            scope: InstallScope::Machine,
        },
        UiAction::SetComponent {
            component: ComponentId::new("docs").expect("id"),
            selected: true,
        },
        UiAction::SetInstallDirectory {
            directory: r"C:\Apps\Acme".into(),
        },
        UiAction::ResetInstallDirectory,
        UiAction::Install,
        UiAction::Update,
        UiAction::Modify,
        UiAction::Repair,
        UiAction::RequestUninstall,
        UiAction::ConfirmUninstall,
        UiAction::DismissUninstall,
        UiAction::Cancel,
        UiAction::Retry,
        UiAction::OpenLog,
        UiAction::CopyDiagnostics,
        UiAction::Launch,
        UiAction::Close,
    ];
    for action in actions {
        let frame = envelope(UiMessage::Action(action));
        let bytes = encode(&frame).expect("encoded");
        assert_eq!(decode(&bytes).expect("decoded"), frame);
    }
}

fn decode_state(bytes: &str) -> UiState {
    serde_json::from_str(bytes).expect("a state")
}

#[test]
fn a_snapshot_carries_the_whole_presentation_and_not_a_delta() {
    let mut state = snapshot();
    state.state = UiState::Running;
    state.progress = Some(ProgressPresentation::new(
        OperationPhase::Files,
        400,
        1000,
        "Installing files",
    ));
    state.diagnostic = Some(DiagnosticPresentation {
        kind: DiagnosticKind::Blocked,
        title: "An application is still running".into(),
        meaning: "Close it first.".into(),
        recovery: "Then retry.".into(),
        technical_details: None,
    });
    let frame = envelope(UiMessage::Snapshot(Box::new(state)));
    let decoded = decode(&encode(&frame).expect("encoded")).expect("decoded");
    let UiMessage::Snapshot(received) = decoded.message else {
        panic!("a snapshot")
    };
    assert_eq!(received.state, UiState::Running);
    assert_eq!(received.progress.expect("progress").percent(), Some(40));
    assert!(matches!(received.surface, UiSurface::Install(_)));
}

#[test]
fn a_maintenance_snapshot_reports_what_can_be_done_to_an_installation() {
    let frame = envelope(UiMessage::Snapshot(Box::new(UiSnapshot {
        surface: UiSurface::Maintenance(MaintenanceState {
            installed_version: "1.3.0".into(),
            components: Vec::new(),
            groups: Vec::new(),
            updates_enabled: true,
            scope: InstallScope::Machine,
            install_directory: Some(r"C:\Program Files\Acme".into()),
            health: zup_ui_protocol::InstallationHealth::UpToDate,
        }),
        ..snapshot()
    })));
    let decoded = decode(&encode(&frame).expect("encoded")).expect("decoded");
    let UiMessage::Snapshot(received) = decoded.message else {
        panic!("a snapshot")
    };
    let UiSurface::Maintenance(maintenance) = received.surface else {
        panic!("a maintenance surface")
    };
    assert!(maintenance.updates_enabled);
    assert_eq!(
        maintenance.health,
        zup_ui_protocol::InstallationHealth::UpToDate
    );
}

#[test]
fn a_component_id_is_never_empty() {
    assert!(ComponentId::new("   ").is_err());
    assert_eq!(ComponentId::new(" docs ").expect("id").as_str(), "docs");
    let bytes = serde_json::to_string(&ComponentId::new("docs").expect("id")).expect("encoded");
    assert_eq!(bytes, "\"docs\"");
    assert!(serde_json::from_str::<ComponentId>("\"\"").is_err());
}
