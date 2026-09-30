//! A minimal preset, spawned by this crate's own tests.
//!
//! It is not a mock: it collects the endpoint a host created, reads the host's
//! hello, and answers with the host hello a real host sends. That is the whole
//! of the handshake, and a transport whose test peer does exactly the handshake
//! has exercised the same path a shipped preset takes.

use zup_ui_ipc::Bootstrap;
use zup_ui_protocol::{
    HostHello, UI_PROTOCOL_VERSION, UiAction, UiCapabilities, UiCapability, UiEnvelope, UiMessage,
    UiSessionId,
};

fn main() {
    let channel = Bootstrap::from_arguments(std::env::args().skip(1))
        .expect("a preset is launched with an endpoint")
        .collect()
        .expect("collect the endpoint");

    let session = UiSessionId(channel.session());
    let hello = channel.recv().expect("the host opens the session");
    let UiMessage::UiHello(greeting) = &hello.message else {
        panic!("the host opens with a hello: {:?}", hello.message);
    };
    assert_eq!(greeting.session, session, "the host speaks our session");
    assert_eq!(
        greeting.protocol_version, UI_PROTOCOL_VERSION,
        "both sides state the same protocol version"
    );

    let mut sequence = 0;
    let mut frame = |message: UiMessage| {
        sequence += 1;
        UiEnvelope {
            version: UI_PROTOCOL_VERSION,
            session,
            sequence,
            message,
        }
    };

    channel
        .sender()
        .send(&frame(UiMessage::HostHello(HostHello::new(
            session,
            UiCapabilities::new([UiCapability::Components]),
            zup_ui_protocol::ProductIdentity {
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
            UiMessage::Closed => break,
            UiMessage::Action(UiAction::Close) => {
                channel
                    .sender()
                    .send(&frame(UiMessage::Closed))
                    .expect("the preset ends the session");
                break;
            }
            _ => {}
        }
    }
}
