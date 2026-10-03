//! What an application configured, and the limits a host enforces before a
//! preset ever sees it.
//!
//! A configuration carries two things an application author wrote: settings
//! the preset deserializes into its own types, and asset names the host has
//! already resolved to files. Neither is a path a person typed at runtime and
//! neither is a path the preset supplied, which is the point: the host
//! materializes both, and a preset that received an arbitrary path would be
//! reading a file nobody authorized.

use zup_preset_protocol::{ConfigurationError, MAX_ASSET_PATH_BYTES, MAX_ASSETS, Configuration};

/// Nothing configured is a valid configuration, not a missing one.
#[test]
fn a_configuration_may_be_empty() {
    Configuration::empty().validate().expect("empty is valid");
}

/// The name a preset uses to ask for an asset is what the host resolves, so the
/// mapping is by name and not by index or by insertion order.
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

/// An application cannot make the host carry an unbounded table. A preset that
/// trusts `assets.len()` as a render budget is only safe because this refuses.
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

/// An empty name would make two assets indistinguishable, and a preset has no
/// way to tell which one an `AssetRef` meant.
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

/// A path longer than anything the host writes is not a path this host wrote.
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

/// Settings are the application's data and travel as JSON, because the
/// protocol cannot know a preset's `Settings` type. Inventing a value type here
/// would be a second, untyped protocol layered on the first.
#[test]
fn settings_travel_as_json_the_preset_deserializes_itself() {
    let mut configuration = Configuration::empty();
    configuration.settings = serde_json::json!({ "accent": "#695cff", "logo": "logo" });
    let bytes = serde_json::to_vec(&configuration).expect("serializes");
    let read: Configuration = serde_json::from_slice(&bytes).expect("deserializes");
    assert_eq!(read.settings["accent"], "#695cff");
    assert_eq!(read, configuration);
}

/// A field this version does not define is a refusal, not a field to skip: a
/// configuration from a newer host is not one this preset can half-follow.
#[test]
fn a_configuration_with_an_unknown_field_is_refused() {
    let value = serde_json::json!({
        "settings": {},
        "assets": {},
        "somethingNewer": 1,
    });
    assert!(serde_json::from_value::<Configuration>(value).is_err());
}
