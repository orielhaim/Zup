//! A real preset, launched as a child process by the installer's end-to-end test.
//!
//! It depends on the public `zup-preset-sdk` and on nothing else, the way a
//! third-party preset project does. It does not open a window: the only thing
//! between "the host launched a preset" and "a person sees a window" is GPUI's
//! own startup, and a headless environment has no display to start one on.
//! Everything a host can observe happens before that point, and all of it is what
//! the test is about.
//!
//! It writes what it received to the path its own settings named. That is not a
//! test hook: a preset reading its settings and acting on them is the entire
//! point of the configuration, and a report written to a file the application
//! chose is the honest way to observe it from another process.

use std::fmt::Write as _;
use std::path::PathBuf;

use zup_preset_protocol::{Action, Capabilities, Capability, InstallerState};
use zup_preset_sdk::{Bootstrap, Channel, Identity};

/// What this preset accepts.
///
/// Typed, so a document that does not fit it fails here rather than travelling on
/// as opaque JSON the preset quietly ignores.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct Settings {
    hero: Option<String>,
    logo: Option<zup_preset_sdk::AssetRef>,
    /// Where this preset reports what it received.
    report: PathBuf,
}

fn main() {
    let bootstrap = Bootstrap::from_arguments(std::env::args().skip(1))
        .expect("a preset is launched with an endpoint");
    let opened = Channel::open(
        &bootstrap,
        Identity {
            name: "e2e".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            required_capabilities: Capabilities::default(),
        },
    )
    .unwrap_or_else(|error| {
        panic!("the handshake completes: {error}");
    });

    let settings: Settings = serde_json::from_value(opened.configuration.settings.clone())
        .unwrap_or_else(|error| {
            eprintln!("settings arrived as {}", opened.configuration.settings);
            panic!("the host's settings fit this preset's own type: {error}");
        });

    let mut report = String::new();
    let _ = writeln!(report, "hero={}", settings.hero.clone().unwrap_or_default());
    let _ = writeln!(report, "host={}", opened.host.product.name);
    let _ = writeln!(report, "state={:?}", opened.snapshot.state);
    let _ = writeln!(
        report,
        "maintenance={}",
        opened.host.capabilities.contains(Capability::Maintenance)
    );
    let _ = writeln!(report, "protocol={}", opened.host.protocol_version);
    let _ = writeln!(
        report,
        "components={}",
        opened.snapshot.surface.components().len()
    );
    for name in opened.configuration.assets.keys() {
        let path = &opened.configuration.assets[name];
        let bytes = std::fs::read(path).expect("the host materialized the asset");
        let _ = writeln!(report, "asset={name}");
        let _ = writeln!(report, "asset-bytes={}", bytes.len());
    }
    std::fs::write(&settings.report, report).expect("the preset reports what it received");

    let surface = opened.snapshot.surface.clone();
    // What a person does: choose what to install, then start it. Both are actions
    // the host validates against the state it owns, and the second is the one
    // that asks the machine to do something.
    if let InstallerState::Options = opened.snapshot.state {
        if let Some(component) = surface.components().iter().find(|c| !c.required) {
            opened.channel.requests().send(Action::SetComponent {
                component: component.id.clone(),
                selected: !component.selected,
            });
        }
        opened.channel.requests().send(Action::Install);
    }

    // And then the window closes, which is how a preset ends a session.
    opened.channel.requests().send(Action::Close);
}
