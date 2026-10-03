//! What a preset says about itself, and what a publisher can do with that
//! without running it.
//!
//! `zup ui pack` is the only moment a preset executable runs, and it runs in the
//! preset author's build. Everything after that - packaging, inspecting,
//! validating an application's settings, composing an installer - has to work
//! from the document alone, so each rule here is tested by breaking the
//! document rather than by describing a good one.

use zup_preset_protocol::{
    DESCRIBE_FLAG, DescribeError, MAX_DESCRIBE_BYTES, PresetDescription, PRESET_PROTOCOL_VERSION,
    Capabilities, Capability,
};

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "accent": { "type": "string" } },
    })
}

fn description() -> PresetDescription {
    PresetDescription::new("aurora", "1.2.0", schema()).with_capabilities(Capabilities::new([
        Capability::Components,
        Capability::PlanPreview,
    ]))
}

/// A preset that is ready to package describes itself completely.
#[test]
fn a_preset_describes_itself_completely() {
    let described = description();
    described.validate().expect("a described preset is valid");
    assert_eq!(described.ui_protocol, PRESET_PROTOCOL_VERSION);
    assert_eq!(described.name, "aurora");
    assert_eq!(
        described.required_capabilities.names(),
        ["components", "plan-preview"]
    );
}

/// A preset that configures nothing is still described completely: the absence
/// of capabilities is a statement, and it is a different one from requiring them.
#[test]
fn a_preset_that_needs_nothing_says_so() {
    let plain = PresetDescription::new("plain", "0.1.0", schema());
    plain.validate().expect("a described preset is valid");
    assert!(plain.required_capabilities.is_empty());
}

/// The describe flag is the whole of the contract between `zup ui pack` and a
/// preset executable, so it is a constant rather than a string repeated at both
/// ends.
#[test]
fn the_describe_flag_is_one_name() {
    assert_eq!(DESCRIBE_FLAG, "--zup-describe");
}

/// A document that does not say which preset it is cannot become a package.
#[test]
fn a_document_without_an_identity_is_refused() {
    let mut blank = description();
    blank.name = "  ".into();
    assert_eq!(blank.validate(), Err(DescribeError::EmptyName));

    let mut unversioned = description();
    unversioned.version = String::new();
    assert_eq!(unversioned.validate(), Err(DescribeError::EmptyVersion));
}

/// A settings schema a validator could not compile against is refused here
/// rather than at the first application that uses this preset.
#[test]
fn a_settings_schema_that_is_not_an_object_is_refused() {
    let mut broken = description();
    broken.settings_schema = serde_json::json!(["not", "a", "schema"]);
    assert_eq!(
        broken.validate(),
        Err(DescribeError::SettingsSchemaNotAnObject)
    );
}

/// The describe document round-trips through JSON unchanged, because the whole
/// of `zup ui pack` is a process printing it and another process reading it.
#[test]
fn the_describe_document_survives_json() {
    let described = description();
    let bytes = serde_json::to_vec(&described).expect("serializes");
    let read: PresetDescription = serde_json::from_slice(&bytes).expect("deserializes");
    assert_eq!(read, described);
}

/// An unknown field is a document this build does not understand, not something
/// to ignore. Ignoring it is how a preset that means something different gets
/// packaged as if it meant this.
#[test]
fn a_document_with_an_unknown_field_is_refused() {
    let mut document = serde_json::to_value(description()).expect("serializes");
    document["somethingNewer"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<PresetDescription>(document).is_err());
}

/// The same document always serializes to the same bytes, so two builds of one
/// preset produce packages that can be compared.
#[test]
fn the_describe_document_is_deterministic() {
    assert_eq!(
        serde_json::to_vec(&description()).expect("serializes"),
        serde_json::to_vec(&description()).expect("serializes")
    );
}

/// The bounds are statements about what a generated schema can weigh and how
/// many targets a preset can support, not limits an application meets. A
/// publisher that exceeds one is refused rather than truncating, so they have to
/// be real numbers rather than a comment.
#[test]
fn the_bounds_are_stated_rather_than_unbounded() {
    let generous = PresetDescription::new("aurora", "1.0.0", schema());
    assert!(generous.settings_schema.to_string().len() < MAX_DESCRIBE_BYTES);
    assert!(generous.validate().is_ok(), "a real schema fits the bound");
}
