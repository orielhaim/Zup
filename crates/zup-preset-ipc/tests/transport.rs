use std::path::PathBuf;
use std::process::Command;

use zup_preset_ipc::{Bootstrap, Endpoint, Error};
use zup_preset_protocol::{Envelope, Message, PRESET_PROTOCOL_VERSION, PresetHello, SessionId};

fn preset() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zup-preset-ipc-test-preset"))
}

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
