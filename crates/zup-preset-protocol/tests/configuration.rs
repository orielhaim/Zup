use zup_preset_protocol::{Configuration, ConfigurationError, MAX_ASSET_PATH_BYTES, MAX_ASSETS};

#[test]
fn a_configuration_may_be_empty() {
    Configuration::empty().validate().expect("empty is valid");
}

#[test]
fn assets_are_resolved_by_name() {
    let mut configuration = Configuration::empty();
    configuration
        .assets
        .insert("logo".into(), "C:/Temp/zup/ui/logo.svg".into());
    configuration
        .assets
        .insert("hero".into(), "C:/Temp/zup/ui/hero.png".into());
    configuration.validate().expect("valid");
    assert_eq!(
        configuration.assets.get("logo").map(String::as_str),
        Some("C:/Temp/zup/ui/logo.svg")
    );
    assert!(!configuration.assets.contains_key("missing"));
}

#[test]
fn more_assets_than_the_limit_are_refused() {
    let mut configuration = Configuration::empty();
    for index in 0..=MAX_ASSETS {
        configuration
            .assets
            .insert(format!("asset-{index}"), "C:/Temp/a".into());
    }
    assert_eq!(
        configuration.validate(),
        Err(ConfigurationError::TooManyAssets {
            count: MAX_ASSETS + 1,
            limit: MAX_ASSETS,
        })
    );
}

#[test]
fn an_empty_asset_name_is_refused() {
    let mut configuration = Configuration::empty();
    configuration
        .assets
        .insert(String::new(), "C:/Temp/a".into());
    assert_eq!(
        configuration.validate(),
        Err(ConfigurationError::EmptyAssetName)
    );
}

#[test]
fn an_over_long_asset_path_is_refused() {
    let mut configuration = Configuration::empty();
    configuration
        .assets
        .insert("logo".into(), "x".repeat(MAX_ASSET_PATH_BYTES + 1));
    assert_eq!(
        configuration.validate(),
        Err(ConfigurationError::AssetPathTooLong {
            name: "logo".into(),
            limit: MAX_ASSET_PATH_BYTES,
        })
    );
}

#[test]
fn settings_travel_as_json_the_preset_deserializes_itself() {
    let mut configuration = Configuration::empty();
    configuration.settings = serde_json::json!({ "accent": "#695cff", "logo": "logo" });
    let bytes = serde_json::to_vec(&configuration).expect("serializes");
    let read: Configuration = serde_json::from_slice(&bytes).expect("deserializes");
    assert_eq!(read.settings["accent"], "#695cff");
    assert_eq!(read, configuration);
}

#[test]
fn a_configuration_with_an_unknown_field_is_refused() {
    let value = serde_json::json!({
        "settings": {},
        "assets": {},
        "somethingNewer": 1,
    });
    assert!(serde_json::from_value::<Configuration>(value).is_err());
}
