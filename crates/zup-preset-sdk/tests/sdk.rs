//! The SDK's own contract, tested without a host.
//!
//! The transport needs a real pipe and a real installer, so it is exercised by
//! the default preset's integration tests and by `zup preset dev` rather than here.
//! What *is* here is everything a preset author depends on before any of that
//! happens: the bootstrap it is launched with, the describe document its build
//! reads, and the schema that document carries.
//!
//! A preset that is wrong in any of these ways fails in a way that looks like
//! somebody else's bug - a build that cannot find a schema, a host that cannot
//! read a session id - so each is checked by breaking it here rather than
//! discovered there.

use zup_preset_ipc::Bootstrap;
use zup_preset_sdk::prelude::*;
use zup_preset_sdk::{Describe, PresetContext, describe};
use zup_sdk::__private::schemars as _;
use zup_sdk::__private::serde as _;

/// A preset with the settings a real one has. The fields are read by the build,
/// not here, so their only job is to appear in a generated schema.
#[zup_preset_sdk::settings]
#[allow(dead_code)]
struct Branded {
    hero: Option<String>,
    accent: Option<String>,
    logo: Option<AssetRef>,
}

struct Aurora;

impl Preset for Aurora {
    const NAME: &'static str = "aurora";
    const VERSION: &'static str = "1.4.2";

    type Settings = Branded;

    fn required_capabilities() -> Capabilities {
        Capabilities::new([Capability::Components, Capability::PlanPreview])
    }

    fn launch(_context: PresetContext<Self::Settings>, _cx: &mut App) {}
}

/// A preset that configures nothing and requires nothing.
struct Plain;

impl Preset for Plain {
    const NAME: &'static str = "plain";
    const VERSION: &'static str = "0.1.0";
    type Settings = NoSettings;
    fn launch(_context: PresetContext<Self::Settings>, _cx: &mut App) {}
}

/// A preset whose name is not a name. It exists to be refused.
struct Unnamed;

impl Preset for Unnamed {
    const NAME: &'static str = "  ";
    const VERSION: &'static str = "0.1.0";
    type Settings = NoSettings;
    fn launch(_context: PresetContext<Self::Settings>, _cx: &mut App) {}
}

/// The bootstrap is the whole of a preset's command line: one name.
///
/// Everything else - the session, the settings, the assets - arrives as protocol
/// messages, so there is nothing else here for a preset to be started with and
/// nothing here that identifies what is being installed.
#[test]
fn a_bootstrap_is_the_endpoint_name_alone() {
    let argument = Bootstrap::to_argument("zup-1f0a9c");
    assert_eq!(argument, "--zup-endpoint=zup-1f0a9c");
    assert!(
        Bootstrap::from_arguments([argument]).is_ok(),
        "the host's own endpoint parses"
    );
}

/// A preset launched without a bootstrap has nothing to connect to, and saying
/// so beats connecting to whatever answers.
#[test]
fn a_preset_launched_without_a_bootstrap_cannot_connect() {
    for arguments in [
        vec![],
        vec!["--zup-pipe=zup-1f0a9c".to_owned()],
        vec!["--zup-session=not-a-uuid".to_owned()],
        vec!["--zup-host=1".to_owned()],
    ] {
        assert!(Bootstrap::from_arguments(arguments).is_err());
    }
}

/// Arguments that are not the bootstrap are ignored, because a host may pass
/// more than the bootstrap and a preset must not refuse to start over one.
#[test]
fn arguments_that_are_not_the_bootstrap_are_ignored() {
    let arguments = vec![
        "--zup-nonsense".to_owned(),
        Bootstrap::to_argument("zup-abc"),
        "extra".into(),
    ];
    assert!(Bootstrap::from_arguments(arguments).is_ok());
}

/// The describe document is what `zup preset pack` reads, so it has to name the
/// preset, its contract, and its settings schema.
#[test]
fn a_preset_describes_its_settings_schema() {
    let document: zup_preset_protocol::PresetDescription =
        serde_json::from_str(&describe::<Aurora>().expect("describes")).expect("reads");
    document.validate().expect("a described preset is valid");
    assert_eq!(document.name, "aurora", "a preset reports its own identity");
    assert_eq!(document.version, "1.4.2");
    assert_eq!(
        document.ui_protocol,
        zup_preset_protocol::PRESET_PROTOCOL_VERSION
    );
    assert_eq!(
        document.required_capabilities.names(),
        ["components", "plan-preview"]
    );
    assert_eq!(
        document.settings_schema["properties"]["hero"]["type"],
        serde_json::json!(["string", "null"]),
        "an optional setting is a string or absent, and the schema says so"
    );
    assert_eq!(
        document.settings_schema["properties"]["accent"]["type"],
        serde_json::json!(["string", "null"])
    );
}

/// An `AssetRef` is what tells the build that a setting is a file rather than a
/// string. The build reads the schema to find them, so the marker has to be on
/// the schema and not only in the Rust type.
#[test]
fn an_asset_setting_is_marked_in_the_schema() {
    let document: zup_preset_protocol::PresetDescription =
        serde_json::from_str(&describe::<Aurora>().expect("describes")).expect("reads");
    let asset = &document.settings_schema["$defs"]["AssetRef"];
    assert_eq!(asset["type"], "string", "an asset is named by a string");
    assert_eq!(
        asset["x-zup-asset"], true,
        "the build finds assets by this marker, so it has to reach the schema"
    );
}

/// A string that is not an asset carries no marker, so the build does not go
/// looking for a file a caller meant as text.
#[test]
fn a_string_setting_is_not_marked_as_an_asset() {
    let document: zup_preset_protocol::PresetDescription =
        serde_json::from_str(&describe::<Aurora>().expect("describes")).expect("reads");
    assert!(
        document.settings_schema["properties"]["hero"]
            .get("x-zup-asset")
            .is_none()
    );
}

/// A preset with no settings is a normal preset, and its schema still describes
/// a document an application can write.
#[test]
fn a_preset_with_no_settings_describes_an_empty_object() {
    let document: zup_preset_protocol::PresetDescription =
        serde_json::from_str(&describe::<Plain>().expect("describes")).expect("reads");
    document
        .validate()
        .expect("a settings-free preset is valid");
    assert_eq!(document.name, "plain");
    assert_eq!(document.settings_schema["type"], "object");
    assert_eq!(document.settings_schema["additionalProperties"], false);
    assert!(document.required_capabilities.is_empty());
}

/// A preset that requires nothing connects to any host, which is the default and
/// the common case.
#[test]
fn a_preset_requires_nothing_by_default() {
    assert!(Plain::required_capabilities().is_empty());
}

/// A preset that could never be packaged says so from the command its author
/// ran, rather than emitting a document the publisher has to reject on its
/// behalf.
#[test]
fn a_preset_that_cannot_describe_itself_is_refused() {
    let error = describe::<Unnamed>().expect_err("a nameless preset is not publishable");
    assert!(
        matches!(error, Describe::Unusable(_)),
        "a refusal, not a document: {error}"
    );
    assert!(error.to_string().contains("names no preset"), "{error}");
}

/// The same preset always describes itself identically, so two builds of one
/// preset produce packages that can be compared.
#[test]
fn a_preset_describes_itself_the_same_way_twice() {
    assert_eq!(
        describe::<Aurora>().expect("describes"),
        describe::<Aurora>().expect("describes")
    );
}

/// An `AssetRef` is a name an application wrote, and it round-trips as one.
#[test]
fn an_asset_reference_is_the_name_an_application_wrote() {
    let asset = AssetRef::new("branding/logo.svg");
    assert_eq!(asset.as_str(), "branding/logo.svg");
    assert_eq!(asset.to_string(), "branding/logo.svg");
    let read: AssetRef =
        serde_json::from_value(serde_json::json!("branding/logo.svg")).expect("reads");
    assert_eq!(read, asset);
}
