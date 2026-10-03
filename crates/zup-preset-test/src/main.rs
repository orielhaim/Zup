//! A real preset, launched as a child process by the installer's end-to-end test.
//!
//! It does not open a window: the only thing between "the host launched a
//! preset" and "a person sees a window" is GPUI's own startup, and a headless
//! environment has no display to start one on. Everything a host can observe
//! happens before that point, and all of it is what the test is about.
//!
//! It drives the transport and the contract directly rather than going through
//! the preset SDK's session layer. That is deliberate, and it is what makes the
//! test worth having: a host checked against a peer that shares its own session
//! code cannot catch a host and an SDK that disagree about the wire, because the
//! disagreement cancels itself out. A third-party preset would depend on
//! `zup-sdk` instead, as `zup preset init` generates.
//!
//! It writes what it received to the path its own settings named. That is not a
//! test hook: a preset reading its settings and acting on them is the entire
//! point of the configuration, and a report written to a file the application
//! chose is the honest way to observe it from another process.

use std::fmt::Write as _;
use std::path::PathBuf;

use zup_preset_ipc::Bootstrap;
use zup_preset_protocol::{
    Action, Capabilities, Capability, Configuration, Envelope, Handshake, InstallerState, Message,
    PresetDescription, Session, SessionProgress, Snapshot,
};

/// What this preset accepts.
///
/// Typed, so a document that does not fit it fails here rather than travelling on
/// as opaque JSON the preset quietly ignores.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct Settings {
    hero: Option<String>,
    logo: Option<String>,
    /// Where this preset reports what it received.
    report: PathBuf,
}

/// The identity this preset calls itself, and what it needs to present.
///
/// Nothing: a preset that can draw whatever the host has needs no capability,
/// and one that needs something states it here so a host that cannot provide it
/// refuses the preset rather than launching it with a dead control.
fn identity() -> (String, String, Capabilities) {
    (
        env!("CARGO_PKG_NAME").to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
        Capabilities::default(),
    )
}

/// The document `zup preset pack` reads to describe this preset.
fn describe() -> String {
    // The same shape the SDK's `#[settings]` generates: `report` is required,
    // because without it the preset has nowhere to report what it received.
    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "hero": { "type": ["string", "null"] },
            "logo": { "type": ["string", "null"] },
            "report": { "type": "string" },
        },
        "required": ["report"],
        "additionalProperties": false,
    });
    let (name, version, capabilities) = identity();
    let document = PresetDescription::new(name, version, schema).with_capabilities(capabilities);
    document.validate().expect("this description is valid");
    serde_json::to_string_pretty(&document).expect("a description is json")
}

fn main() {
    // The describe mode a publisher asks for. It is answered before anything
    // else, because it is the only time this preset produces a document rather
    // than act on one, and it must not need a host to be running.
    if std::env::args()
        .skip(1)
        .any(|argument| argument == zup_preset_protocol::DESCRIBE_FLAG)
    {
        println!("{}", describe());
        return;
    }

    let bootstrap = Bootstrap::from_arguments(std::env::args().skip(1))
        .expect("a preset is launched with an endpoint");
    let channel = bootstrap.collect().expect("the endpoint is collected");

    // The session id is the one the transport handed over, not a fresh one: both
    // peers have to be talking about the same connection, and the host checks
    // every frame against the id it created.
    let (name, version, required) = identity();
    let mut session = Session::preset(channel.session().into(), required, name, version);

    let greeting = session.opening().expect("a preset opens with a hello");
    channel
        .sender()
        .send(&greeting)
        .expect("the host is listening");

    // Read until the host has told this preset what to draw. The configuration
    // arrives with the first snapshot, and a preset that reconnects or starts
    // late still ends up with both, because a snapshot is the whole state.
    let mut configuration = Configuration::empty();
    let mut capabilities = Capabilities::default();
    let mut product = None;
    let mut snapshot: Option<Snapshot> = None;
    while snapshot.is_none() {
        let frame = channel
            .recv()
            .expect("the host keeps talking until it sends a snapshot");
        // The host's answer carries what it can do, and a configuration that
        // arrives separately carries what the application chose. Both are read
        // out of the frames rather than from the session, because the session
        // keeps only what a caller has to act on.
        match &frame.message {
            Message::HostHello(hello) => {
                capabilities = hello.capabilities.clone();
                product = Some(hello.product.clone());
            }
            Message::Configuration(configuration_value) => {
                configuration = (**configuration_value).clone()
            }
            _ => {}
        }
        let outgoing = session
            .receive(frame)
            .expect("the host follows the protocol");
        if let SessionProgress::Send(outgoing) = outgoing {
            channel
                .sender()
                .send(&outgoing)
                .expect("the host is listening");
        }
        if let Handshake::Live { snapshot: live } = session.handshake() {
            snapshot = Some((**live).clone());
        }
    }
    let snapshot = snapshot.expect("the loop runs until there is a snapshot");

    let settings: Settings =
        serde_json::from_value(configuration.settings.clone()).unwrap_or_else(|error| {
            eprintln!("settings arrived as {}", configuration.settings);
            panic!("the host's settings fit this preset's own type: {error}");
        });

    let mut report = String::new();
    let _ = writeln!(report, "hero={}", settings.hero.clone().unwrap_or_default());
    // The identity the host says it is presenting. A preset draws this, so it is
    // worth reporting whether it arrived.
    if let Some(product) = product {
        let _ = writeln!(report, "host={}", product.name);
        let _ = writeln!(report, "version={}", product.version);
    }
    let _ = writeln!(report, "state={:?}", snapshot.state);
    let _ = writeln!(report, "components={}", snapshot.surface.components().len());
    let _ = writeln!(
        report,
        "maintenance={}",
        capabilities.contains(Capability::Maintenance)
    );
    let _ = writeln!(
        report,
        "protocol={}",
        zup_preset_protocol::PRESET_PROTOCOL_VERSION
    );
    for (name, path) in &configuration.assets {
        let bytes = std::fs::read(path).expect("the host materialized the asset");
        let _ = writeln!(report, "asset={name}");
        let _ = writeln!(report, "asset-bytes={}", bytes.len());
    }
    std::fs::write(&settings.report, report).expect("the preset reports what it received");

    // What a person does: choose what to install, then start it. Both are actions
    // the host validates against the state it owns, and the second is the one that
    // asks the machine to do something.
    let asking = InstallerState::Options == snapshot.state;
    let mut pending: Vec<Envelope> = Vec::new();
    if asking && let Some(component) = snapshot.surface.components().iter().find(|c| !c.required) {
        pending.push(
            session
                .frame(Message::Action(Action::SetComponent {
                    component: component.id.clone(),
                    selected: !component.selected,
                }))
                .expect("a preset may ask for this"),
        );
    }
    if asking {
        pending.push(
            session
                .frame(Message::Action(Action::Install))
                .expect("a preset may ask for this"),
        );
    }

    // And then the window closes, which is how a preset ends a session.
    pending.push(session.frame(Message::Closed).expect("a preset may close"));

    let sender = channel.sender().clone();
    std::thread::Builder::new()
        .name("zup-preset-test-actions".into())
        .spawn(move || {
            for frame in pending {
                if sender.send(&frame).is_err() {
                    return;
                }
            }
        })
        .expect("the preset can write its own requests")
        .join()
        .expect("the requests are sent");
}
