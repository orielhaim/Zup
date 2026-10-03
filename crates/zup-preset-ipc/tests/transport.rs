//! The transport, exercised across a real process boundary.
//!
//! A transport that only ever meets its own peer in the same process has proved
//! that two halves of a struct fit together. These tests spawn a second process
//! that collects the endpoint and performs the real handshake, which is the only
//! way to find out whether a frame survives an actual serialisation boundary and
//! whether the session id actually arrives.

use std::path::PathBuf;
use std::process::Command;

use zup_preset_ipc::{Bootstrap, Endpoint, Error};
use zup_preset_protocol::{PRESET_PROTOCOL_VERSION, Envelope, PresetHello, Message, SessionId};

/// The child is this crate's own preset, built behind a test-only feature, so the
/// peer is a real preset that does the real handshake.
fn preset() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zup-preset-ipc-test-preset"))
}

/// A preset is launched with one argument and nothing else.
#[test]
fn a_bootstrap_is_one_argument() {
    let endpoint = Endpoint::create().expect("create an endpoint");
    let argument = Bootstrap::to_argument(endpoint.name());
    assert_eq!(argument, format!("--zup-endpoint={}", endpoint.name()));
    assert!(
        Bootstrap::from_arguments([argument]).is_ok(),
        "the host's own endpoint parses"
    );
}

/// A preset that is not launched by a host has nothing to connect to, and says
/// so rather than trying.
#[test]
fn a_preset_launched_without_an_endpoint_cannot_connect() {
    for arguments in [
        vec![],
        vec!["--zup-nonsense".to_owned()],
        vec!["--zup-endpoint".to_owned()],
        vec!["--zup-session=not-an-endpoint".to_owned()],
    ] {
        assert!(
            matches!(
                Bootstrap::from_arguments(arguments),
                Err(Error::NotLaunchedByAHost)
            ),
            "a preset must refuse to run without an endpoint"
        );
    }
}

/// Arguments a host did not write are ignored, because a host may pass more.
#[test]
fn arguments_that_are_not_the_endpoint_are_ignored() {
    let endpoint = Endpoint::create().expect("create an endpoint");
    let arguments = vec![
        "--zup-nonsense".to_owned(),
        Bootstrap::to_argument(endpoint.name()),
        "extra".to_owned(),
    ];
    assert!(Bootstrap::from_arguments(arguments).is_ok());
}

/// A frame survives the process boundary in both directions, carrying the
/// session both sides agreed on.
///
/// The child asserts the session matches and states the same protocol version,
/// and answers with the host hello a real host sends; a session id that did not
/// survive the boundary would panic in the child rather than pass here.
#[test]
fn a_frame_survives_a_real_process_boundary() {
    let host = Endpoint::create().expect("create an endpoint");
    let mut child = Command::new(preset())
        .arg(Bootstrap::to_argument(host.name()))
        .spawn()
        .expect("spawn the preset");
    let channel = host.accept().expect("the preset collected the endpoint");
    let session = SessionId(channel.session());

    channel
        .sender()
        .send(&Envelope {
            version: PRESET_PROTOCOL_VERSION,
            session,
            sequence: 1,
            message: Message::PresetHello(PresetHello {
                protocol_version: PRESET_PROTOCOL_VERSION,
                session,
                preset: "zup-preset-ipc".into(),
                preset_version: "0.0.0".into(),
                required_capabilities: Default::default(),
            }),
        })
        .expect("the host opens the session");

    let answer = channel.recv().expect("the preset answers");
    assert_eq!(answer.session, session, "the child speaks our session");
    assert_eq!(answer.sequence, 1, "the child numbers its own frames");
    let Message::HostHello(hello) = &answer.message else {
        panic!("the child answers with a host hello: {:?}", answer.message);
    };
    assert_eq!(hello.session, session);
    assert_eq!(hello.protocol_version, PRESET_PROTOCOL_VERSION);

    channel
        .sender()
        .send(&Envelope {
            version: PRESET_PROTOCOL_VERSION,
            session,
            sequence: 2,
            message: Message::Closed,
        })
        .expect("the host ends the session");

    let _ = child.wait();
}

/// Each endpoint is its own name, so a stale name from a finished session cannot
/// be replayed into a new one.
#[test]
fn an_endpoint_is_its_own_name() {
    let first = Endpoint::create().expect("create an endpoint");
    let second = Endpoint::create().expect("create an endpoint");
    assert_ne!(
        first.name(),
        second.name(),
        "two live endpoints must not be reachable at the same name"
    );
}
