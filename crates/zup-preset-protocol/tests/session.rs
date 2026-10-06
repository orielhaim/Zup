//! A whole session, driven by two [`Session`] values and nothing else.
//!
//! No pipe, no window, no runtime. A host and a preset running the same state
//! machine is what makes "the same protocol in production and in `zup preset dev`"
//! a fact rather than an intention, and it can only be checked if a test can
//! drive one end of a session without a process.
//!
//! Each case breaks one rule and asserts the refusal, because a handshake that
//! is only ever tested in the order it is supposed to happen proves nothing
//! about what a preset that does not follow it gets.

use zup_preset_protocol::{
    Action, Capabilities, Capability, ComponentId, ComponentOption, Configuration, Envelope,
    Handshake, InstallOptions, InstallScope, InstallerState, MaintenanceState, Message,
    PRESET_PROTOCOL_VERSION, PeerRole, PlanStatus, ProductIdentity, Session, SessionId,
    SessionProgress, Snapshot, Surface, WireError,
};

fn product() -> ProductIdentity {
    ProductIdentity {
        name: "Acme".into(),
        publisher: Some("Acme Inc".into()),
        version: "1.4.0".into(),
        description: None,
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        product: product(),
        surface: Surface::Install(InstallOptions {
            existing_version: None,
            scopes: vec![InstallScope::User, InstallScope::Machine],
            scope: InstallScope::User,
            components: vec![ComponentOption {
                id: ComponentId::new("docs").expect("id"),
                name: "Documentation".into(),
                description: None,
                required: false,
                selected: false,
                installed: false,
            }],
            groups: Vec::new(),
            install_directory: None,
            allow_directory_override: true,
        }),
        state: InstallerState::Options,
        operation: None,
        progress: None,
        plan: PlanStatus::Unsupported,
        diagnostic: None,
        update: None,
        repair_drift: Vec::new(),
        launch: None,
    }
}

fn every_capability() -> Capabilities {
    Capabilities::new(Capability::ALL.iter().copied())
}

/// One host and one preset, connected.
struct Pair {
    host: Session,
    preset: Session,
    id: SessionId,
}

impl Pair {
    fn connected() -> Self {
        let id = SessionId::new_v7();
        let mut host = Session::host(id, every_capability(), product());
        let mut preset = Session::preset(
            id,
            Capabilities::new([Capability::Components, Capability::PlanPreview]),
            "aurora".into(),
            "1.0.0".into(),
        );
        let hello = preset.opening().expect("the preset opens");
        let SessionProgress::Send(answer) = host.receive(hello).expect("the host greets") else {
            panic!("the host answers a hello with its own hello");
        };
        let SessionProgress::Done = preset.receive(answer).expect("the preset accepts") else {
            panic!("a preset answers a host hello with nothing");
        };
        Self { host, preset, id }
    }

    /// Push both messages the host publishes, into the preset.
    fn publish(&mut self, configuration: Configuration) -> Vec<Message> {
        let frames = self
            .host
            .publish(configuration, Box::new(snapshot()))
            .expect("publish");
        let mut seen = Vec::new();
        for frame in frames {
            let frame = decode_frame(&frame);
            seen.push(frame.message.clone());
            self.preset.receive(frame).expect("the preset accepts");
        }
        seen
    }
}

fn decode_frame(envelope: &Envelope) -> Envelope {
    let bytes = zup_preset_protocol::encode(envelope).expect("encode");
    zup_preset_protocol::decode(&bytes).expect("decode")
}

/// The two messages a preset must be able to draw from are exactly these, in
/// this order, and a preset that reconnects gets them again from scratch.
#[test]
fn a_connected_preset_receives_its_configuration_then_the_current_state() {
    let mut pair = Pair::connected();
    let published = pair.publish(Configuration::empty());
    assert_eq!(
        published,
        vec![
            Message::Configuration(Box::new(Configuration::empty())),
            Message::Snapshot(Box::new(snapshot())),
        ]
    );
    assert!(matches!(pair.preset.handshake(), Handshake::Live { .. }));
    assert!(matches!(pair.host.handshake(), Handshake::Live { .. }));
}

/// A preset that reconnects gets the whole state again.
///
/// Reconnection is a new connection, so it is a new session: the host's
/// authoritative state is not in the session at all, and publishing into a
/// fresh one hands the new preset everything without replaying anything.
#[test]
fn a_reconnecting_preset_is_sent_the_whole_state_again() {
    let mut pair = Pair::connected();
    pair.publish(Configuration::empty());

    let reconnected = SessionId::new_v7();
    let mut host = Session::host(reconnected, every_capability(), product());
    let mut preset = Session::preset(
        reconnected,
        Capabilities::new([Capability::Components]),
        "aurora".into(),
        "1.0.0".into(),
    );
    let hello = preset.opening().expect("opens");
    let SessionProgress::Send(answer) = host.receive(hello).expect("greets") else {
        panic!("the host answers a hello with its own");
    };
    assert!(preset.receive(answer).is_ok());

    let mut running = snapshot();
    running.state = InstallerState::Running;
    for frame in host
        .publish(Configuration::empty(), Box::new(running.clone()))
        .expect("publishes")
    {
        assert!(preset.receive(frame).is_ok());
    }
    assert!(matches!(
        preset.handshake(),
        Handshake::Live { snapshot } if **snapshot == running
    ));
}

/// A preset that asks for a capability the host does not provide is refused
/// before it can draw anything, rather than being given a window with a dead
/// control in it.
#[test]
fn a_capability_the_host_does_not_provide_stops_the_preset_before_it_runs() {
    let id = SessionId::new_v7();
    let mut host = Session::host(id, Capabilities::new([Capability::Components]), product());
    let mut preset = Session::preset(
        id,
        Capabilities::new([Capability::Maintenance]),
        "aurora".into(),
        "1.0.0".into(),
    );
    let hello = preset.opening().expect("opens");
    assert_eq!(
        host.receive(hello),
        Err(WireError::MissingCapability("maintenance".into()))
    );
}

/// A host that provides what the preset needs is the same handshake with a
/// different set, and a preset that needs nothing at all is still connected.
#[test]
fn a_preset_that_needs_nothing_still_connects() {
    let id = SessionId::new_v7();
    let mut host = Session::host(id, Capabilities::default(), product());
    let mut preset = Session::preset(id, Capabilities::default(), "plain".into(), "0.1.0".into());
    let hello = preset.opening().expect("opens");
    assert!(matches!(host.receive(hello), Ok(SessionProgress::Send(_))));
}

/// The session id is not decoration: a frame from a session this end is not in
/// is refused whatever it says.
#[test]
fn a_frame_from_another_session_is_refused() {
    let mut pair = Pair::connected();
    let stranger = Session::preset(
        SessionId::new_v7(),
        Capabilities::default(),
        "aurora".into(),
        "1.0.0".into(),
    )
    .opening()
    .expect("opens");
    assert_eq!(pair.host.receive(stranger), Err(WireError::SessionMismatch));
}

/// A host cannot answer its own hello, and a preset cannot send one. A peer
/// that mixes the roles up is not a peer this protocol can follow.
#[test]
fn a_message_from_the_wrong_side_is_refused() {
    let mut pair = Pair::connected();
    // A preset that answers the host's hello with another hello. The host is
    // past the handshake, so the refusal has to come from the state, not from
    // the direction: the direction is right and the message is still wrong.
    let second_hello = pair
        .preset
        .frame(Message::PresetHello(zup_preset_protocol::PresetHello {
            protocol_version: PRESET_PROTOCOL_VERSION,
            session: pair.id,
            preset: "aurora".into(),
            preset_version: "1.0.0".into(),
            required_capabilities: Capabilities::default(),
        }))
        .expect("frames");
    assert_eq!(
        pair.host.receive(second_hello),
        Err(WireError::UnexpectedMessage {
            expected: "an action",
            found: "ui_hello",
        })
    );

    let action = pair
        .preset
        .frame(Message::Action(Action::Install))
        .expect("frames");
    assert_eq!(
        pair.preset.receive(action),
        Err(WireError::UnexpectedMessage {
            expected: "host",
            found: "action",
        })
    );
}

/// A version a peer cannot follow is refused in the frame and in the hello, so
/// there is no way to reach a session by lying in only one of the two.
#[test]
fn a_version_this_build_does_not_speak_is_refused() {
    let id = SessionId::new_v7();
    let mut host = Session::host(id, every_capability(), product());
    let stale_frame = Envelope {
        version: PRESET_PROTOCOL_VERSION + 1,
        session: id,
        sequence: 1,
        message: Message::PresetHello(zup_preset_protocol::PresetHello {
            protocol_version: PRESET_PROTOCOL_VERSION,
            session: id,
            preset: "aurora".into(),
            preset_version: "1.0.0".into(),
            required_capabilities: Capabilities::default(),
        }),
    };
    assert_eq!(
        host.receive(stale_frame),
        Err(WireError::VersionMismatch {
            expected: PRESET_PROTOCOL_VERSION,
            found: PRESET_PROTOCOL_VERSION + 1,
        })
    );

    // The same hello, correctly framed but claiming a version the body cannot
    // follow, is refused on the version rather than trusted.
    let mut host = Session::host(id, every_capability(), product());
    assert_eq!(
        host.receive(Envelope {
            version: PRESET_PROTOCOL_VERSION,
            session: id,
            sequence: 1,
            message: Message::PresetHello(zup_preset_protocol::PresetHello {
                protocol_version: PRESET_PROTOCOL_VERSION + 1,
                session: id,
                preset: "aurora".into(),
                preset_version: "1.0.0".into(),
                required_capabilities: Capabilities::default(),
            }),
        }),
        Err(WireError::VersionMismatch {
            expected: PRESET_PROTOCOL_VERSION,
            found: PRESET_PROTOCOL_VERSION + 1,
        })
    );
}

/// A hello that claims a different protocol version than its frame is refused
/// on the version, not on trust.
#[test]
fn a_hello_that_claims_the_wrong_version_is_refused() {
    let id = SessionId::new_v7();
    let mut host = Session::host(id, every_capability(), product());
    let envelope = Envelope {
        version: PRESET_PROTOCOL_VERSION,
        session: id,
        sequence: 1,
        message: Message::PresetHello(zup_preset_protocol::PresetHello {
            protocol_version: PRESET_PROTOCOL_VERSION + 7,
            session: id,
            preset: "aurora".into(),
            preset_version: "1.0.0".into(),
            required_capabilities: Capabilities::default(),
        }),
    };
    assert_eq!(
        host.receive(envelope),
        Err(WireError::VersionMismatch {
            expected: PRESET_PROTOCOL_VERSION,
            found: PRESET_PROTOCOL_VERSION + 7,
        })
    );
}

/// A retransmitted action is how a confirmation ends up applying twice, so the
/// sequence is checked before the message means anything.
#[test]
fn a_replayed_sequence_is_refused() {
    let mut pair = Pair::connected();
    pair.publish(Configuration::empty());
    let first = pair
        .preset
        .frame(Message::Action(Action::Install))
        .expect("frames");
    let sequence = first.sequence;
    let replay = first.clone();
    assert!(matches!(
        pair.host.receive(first),
        Ok(SessionProgress::Act(Action::Install))
    ));
    assert_eq!(
        pair.host.receive(replay),
        Err(WireError::DuplicateSequence { sequence })
    );
}

/// A frame that arrives after a newer one means the transport is not the one
/// this protocol assumes. The preset's counter is read back out of the frames
/// it produced, because a test that hand-writes a sequence number is testing
/// its own arithmetic.
#[test]
fn a_sequence_that_goes_backwards_is_refused() {
    let mut pair = Pair::connected();
    pair.publish(Configuration::empty());
    let first = pair
        .preset
        .frame(Message::Action(Action::Retry))
        .expect("frames");
    let second = pair
        .preset
        .frame(Message::Action(Action::Install))
        .expect("frames");
    let (first_frame, second) = (first.clone(), second.sequence);
    let (first, second) = (first.sequence, second);
    assert!(first > 1, "the hello was the preset's first frame");
    assert!(second > first, "a session's own frames increase");
    assert!(pair.host.receive(first_frame.clone()).is_ok());
    assert_eq!(
        pair.host.receive(first_frame),
        Err(WireError::DuplicateSequence { sequence: first })
    );
    assert_eq!(
        pair.host.receive(Envelope {
            version: PRESET_PROTOCOL_VERSION,
            session: pair.id,
            sequence: first - 1,
            message: Message::Action(Action::Install),
        }),
        Err(WireError::SequenceRegression {
            previous: first,
            next: first - 1,
        })
    );
}

/// Once either side has ended the session, nothing else is accepted: a
/// message that arrives after a close is a peer that did not agree to the
/// close.
#[test]
fn a_closed_session_accepts_nothing_further() {
    let mut pair = Pair::connected();
    pair.publish(Configuration::empty());
    let action = pair
        .preset
        .frame(Message::Action(Action::Install))
        .expect("frames");
    let closed = pair.preset.frame(Message::Closed).expect("frames");
    assert!(!pair.preset.is_closed());
    assert_eq!(
        pair.host.receive(action.clone()),
        Ok(SessionProgress::Act(Action::Install))
    );
    assert_eq!(pair.host.receive(closed), Ok(SessionProgress::Done));
    assert!(pair.host.is_closed());
    assert_eq!(pair.host.receive(action), Err(WireError::SessionClosed));
}

/// Configuration a host could not have produced is refused before a preset sees
/// it, because a preset that trusted a hostile asset path would read a file it
/// was never given.
#[test]
fn a_configuration_beyond_the_limits_is_refused() {
    let mut pair = Pair::connected();
    let mut configuration = Configuration::empty();
    for index in 0..=zup_preset_protocol::MAX_ASSETS {
        configuration
            .assets
            .insert(format!("asset-{index}"), "C:/temp/a.png".into());
    }
    let error = pair
        .host
        .publish(configuration, Box::new(snapshot()))
        .expect_err("too many assets");
    assert!(matches!(
        error,
        WireError::Configuration(zup_preset_protocol::ConfigurationError::TooManyAssets { .. })
    ));
}

/// The role a session runs as is fixed when it is built, which is what lets one
/// type serve both peers without either of them second-guessing the other.
#[test]
fn a_session_knows_which_side_it_is() {
    let id = SessionId::new_v7();
    let host = Session::host(id, Capabilities::default(), product());
    let preset = Session::preset(id, Capabilities::default(), "a".into(), "1".into());
    assert_eq!(host.role(), PeerRole::Host);
    assert_eq!(preset.role(), PeerRole::Preset);
    assert_eq!(host.id(), id);
    assert_eq!(preset.id(), id);
}

/// A maintenance surface reaches a preset exactly as an install surface does:
/// the protocol has no screen names, and the same two messages carry both.
#[test]
fn a_maintenance_session_is_the_same_handshake() {
    let id = SessionId::new_v7();
    let mut host = Session::host(id, every_capability(), product());
    let mut preset = Session::preset(id, Capabilities::default(), "aurora".into(), "1".into());
    let hello = preset.opening().expect("opens");
    let SessionProgress::Send(answer) = host.receive(hello).expect("greets") else {
        panic!("the host answers a hello");
    };
    assert!(preset.receive(answer).is_ok());

    let mut maintenance = snapshot();
    maintenance.surface = Surface::Maintenance(MaintenanceState {
        installed_version: "1.3.0".into(),
        components: Vec::new(),
        groups: Vec::new(),
        updates_enabled: true,
        scope: InstallScope::Machine,
        install_directory: Some("C:/Program Files/Acme".into()),
        health: zup_preset_protocol::InstallationHealth::Drifted {
            resources: vec!["acme.exe".into()],
        },
    });
    maintenance.state = InstallerState::Maintenance;
    for frame in host
        .publish(Configuration::empty(), Box::new(maintenance))
        .expect("publishes")
    {
        assert!(preset.receive(frame).is_ok());
    }
}
