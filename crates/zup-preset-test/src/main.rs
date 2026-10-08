use std::fmt::Write as _;
use std::path::PathBuf;

use zup_preset_ipc::Bootstrap;
use zup_preset_protocol::{
    Action, Capabilities, Capability, Configuration, Envelope, Handshake, InstallerState, Message,
    PresetDescription, Session, SessionProgress, Snapshot,
};

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct Settings {
    hero: Option<String>,
    logo: Option<String>,
    report: PathBuf,
}

fn identity() -> (String, String, Capabilities) {
    (
        env!("CARGO_PKG_NAME").to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
        Capabilities::default(),
    )
}

fn describe() -> String {
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

    let (name, version, required) = identity();
    let mut session = Session::preset(channel.session().into(), required, name, version);

    let greeting = session.opening().expect("a preset opens with a hello");
    channel
        .sender()
        .send(&greeting)
        .expect("the host is listening");

    let mut configuration = Configuration::empty();
    let mut capabilities = Capabilities::default();
    let mut product = None;
    let mut snapshot: Option<Snapshot> = None;
    while snapshot.is_none() {
        let frame = channel
            .recv()
            .expect("the host keeps talking until it sends a snapshot");
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

    pending.push(
        session
            .frame(Message::Action(Action::Close))
            .expect("a preset may ask to close"),
    );

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
