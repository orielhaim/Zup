//! A minimal preset, spawned by this crate's own tests.
//!
//! It is not a mock: it collects the endpoint a host created, reads the host's
//! hello, and answers with the host hello a real host sends. That is the whole
//! of the handshake, and a transport whose test peer does exactly the handshake
//! has exercised the same path a shipped preset takes.

use zup_preset_ipc::Bootstrap;
use zup_preset_protocol::{
    HostHello, PRESET_PROTOCOL_VERSION, Action, Capabilities, Capability, Envelope, Message,
    SessionId,
};

fn main() {
    let channel = Bootstrap::from_arguments(std::env::args().skip(1))
        .expect("a preset is launched with an endpoint")
        .collect()
        .expect("collect the endpoint");

    let session = SessionId(channel.session());
    let hello = channel.recv().expect("the host opens the session");
    let Message::PresetHello(greeting) = &hello.message else {
        panic!("the host opens with a hello: {:?}", hello.message);
    };
    assert_eq!(greeting.session, session, "the host speaks our session");
    assert_eq!(
        greeting.protocol_version, PRESET_PROTOCOL_VERSION,
        "both sides state the same protocol version"
    );

    let mut sequence = 0;
    let mut frame = |message: Message| {
        sequence += 1;
        Envelope {
            version: PRESET_PROTOCOL_VERSION,
            session,
            sequence,
            message,
        }
    };

    channel
        .sender()
        .send(&frame(Message::HostHello(HostHello::new(
            session,
            Capabilities::new([Capability::Components]),
            zup_preset_protocol::ProductIdentity {
                name: "test host".into(),
                publisher: None,
                version: env!("CARGO_PKG_VERSION").into(),
                description: None,
            },
            env!("CARGO_PKG_VERSION"),
        ))))
        .expect("the preset answers the hello");

    while let Ok(incoming) = channel.recv() {
        match incoming.message {
            Message::Closed => break,
            Message::Action(Action::Close) => {
                channel
                    .sender()
                    .send(&frame(Message::Closed))
                    .expect("the preset ends the session");
                break;
            }
            _ => {}
        }
    }
}
