use zup_preset_protocol::{
    Capabilities, Capability, DESCRIBE_FLAG, DescribeError, MAX_DESCRIBE_BYTES,
    PRESET_PROTOCOL_VERSION, PresetDescription,
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

#[test]
fn a_preset_that_needs_nothing_says_so() {
    let plain = PresetDescription::new("plain", "0.1.0", schema());
    plain.validate().expect("a described preset is valid");
    assert!(plain.required_capabilities.is_empty());
}

#[test]
fn the_describe_flag_is_one_name() {
    assert_eq!(DESCRIBE_FLAG, "--zup-describe");
}

#[test]
fn a_document_without_an_identity_is_refused() {
    let mut blank = description();
    blank.name = "  ".into();
    assert_eq!(blank.validate(), Err(DescribeError::EmptyName));

    let mut unversioned = description();
    unversioned.version = String::new();
    assert_eq!(unversioned.validate(), Err(DescribeError::EmptyVersion));
}

#[test]
fn a_settings_schema_that_is_not_an_object_is_refused() {
    let mut broken = description();
    broken.settings_schema = serde_json::json!(["not", "a", "schema"]);
    assert_eq!(
        broken.validate(),
        Err(DescribeError::SettingsSchemaNotAnObject)
    );
}

#[test]
fn the_describe_document_survives_json() {
    let described = description();
    let bytes = serde_json::to_vec(&described).expect("serializes");
    let read: PresetDescription = serde_json::from_slice(&bytes).expect("deserializes");
    assert_eq!(read, described);
}

#[test]
fn a_document_with_an_unknown_field_is_refused() {
    let mut document = serde_json::to_value(description()).expect("serializes");
    document["somethingNewer"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<PresetDescription>(document).is_err());
}

#[test]
fn the_describe_document_is_deterministic() {
    assert_eq!(
        serde_json::to_vec(&description()).expect("serializes"),
        serde_json::to_vec(&description()).expect("serializes")
    );
}

#[test]
fn the_bounds_are_stated_rather_than_unbounded() {
    let generous = PresetDescription::new("aurora", "1.0.0", schema());
    assert!(generous.settings_schema.to_string().len() < MAX_DESCRIBE_BYTES);
    assert!(generous.validate().is_ok(), "a real schema fits the bound");
}
