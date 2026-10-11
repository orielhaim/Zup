#![cfg(feature = "preset")]

use zup_preset_ipc::Bootstrap;
use zup_sdk::__private::schemars as _;
use zup_sdk::__private::serde as _;
use zup_sdk::preset::prelude::*;
use zup_sdk::preset::{Describe, PresetContext, describe};

#[zup_sdk::preset::settings]
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

struct Plain;

impl Preset for Plain {
    const NAME: &'static str = "plain";
    const VERSION: &'static str = "0.1.0";
    type Settings = NoSettings;
    fn launch(_context: PresetContext<Self::Settings>, _cx: &mut App) {}
}

struct Unnamed;

impl Preset for Unnamed {
    const NAME: &'static str = "  ";
    const VERSION: &'static str = "0.1.0";
    type Settings = NoSettings;
    fn launch(_context: PresetContext<Self::Settings>, _cx: &mut App) {}
}

#[test]
fn a_bootstrap_is_the_endpoint_name_alone() {
    let argument = Bootstrap::to_argument("zup-1f0a9c");
    assert_eq!(argument, "--zup-endpoint=zup-1f0a9c");
    assert!(
        Bootstrap::from_arguments([argument]).is_ok(),
        "the host's own endpoint parses"
    );
}

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

#[test]
fn arguments_that_are_not_the_bootstrap_are_ignored() {
    let arguments = vec![
        "--zup-nonsense".to_owned(),
        Bootstrap::to_argument("zup-abc"),
        "extra".into(),
    ];
    assert!(Bootstrap::from_arguments(arguments).is_ok());
}

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

#[test]
fn a_preset_requires_nothing_by_default() {
    assert!(Plain::required_capabilities().is_empty());
}

#[test]
fn a_preset_that_cannot_describe_itself_is_refused() {
    let error = describe::<Unnamed>().expect_err("a nameless preset is not publishable");
    assert!(
        matches!(error, Describe::Unusable(_)),
        "a refusal, not a document: {error}"
    );
    assert!(error.to_string().contains("names no preset"), "{error}");
}

#[test]
fn a_preset_describes_itself_the_same_way_twice() {
    assert_eq!(
        describe::<Aurora>().expect("describes"),
        describe::<Aurora>().expect("describes")
    );
}

#[test]
fn an_asset_reference_is_the_name_an_application_wrote() {
    let asset = AssetRef::new("branding/logo.svg");
    assert_eq!(asset.as_str(), "branding/logo.svg");
    assert_eq!(asset.to_string(), "branding/logo.svg");
    let read: AssetRef =
        serde_json::from_value(serde_json::json!("branding/logo.svg")).expect("reads");
    assert_eq!(read, asset);
}
