//! Unit tests for domain identifiers and extensions.

use rstest::rstest;
use zup_core::{
    AppId, ComponentId, FileExtension, NonEmptyString, PluginId, ProtocolScheme, ValueError,
};

#[rstest]
#[case::app(AppId::new("com.acme.acme"), "com.acme.acme")]
#[case::trims(AppId::new("  com.acme.acme  "), "com.acme.acme")]
fn valid_ids(#[case] result: Result<AppId, ValueError>, #[case] expected: &str) {
    let id = result.unwrap();
    assert_eq!(id.as_str(), expected);
}

#[rstest]
#[case::letter("plugin")]
#[case::digit("9plugin")]
#[case::portable("plugin.name_1-x")]
fn valid_plugin_ids(#[case] value: &str) {
    assert_eq!(PluginId::new(value).unwrap().as_str(), value);
}

#[rstest]
#[case::empty("")]
#[case::leading_dot(".plugin")]
#[case::leading_underscore("_plugin")]
#[case::leading_hyphen("-plugin")]
#[case::space("plugin name")]
#[case::slash("plugin/name")]
#[case::backslash("plugin\\name")]
#[case::non_ascii("plugín")]
fn invalid_plugin_ids(#[case] value: &str) {
    assert!(PluginId::new(value).is_err(), "value: {value}");
}

#[test]
fn empty_ids_rejected() {
    assert_eq!(
        AppId::new("").unwrap_err(),
        ValueError::Empty { kind: "app id" }
    );
    assert_eq!(
        ComponentId::new("   ").unwrap_err(),
        ValueError::Empty {
            kind: "component id"
        }
    );
    assert_eq!(
        NonEmptyString::new("").unwrap_err(),
        ValueError::Empty { kind: "name" }
    );
}

#[rstest]
#[case::simple("acme")]
#[case::with_plus("svn+ssh")]
#[case::with_dash_dot("a.b-c+d")]
fn valid_scheme(#[case] scheme: &str) {
    assert!(ProtocolScheme::new(scheme).is_ok(), "scheme: {scheme}");
}

#[rstest]
#[case::empty("")]
#[case::starts_digit("1acme")]
#[case::has_underscore("ac_me")]
#[case::has_slash("ac/me")]
#[case::has_space("ac me")]
fn invalid_scheme(#[case] scheme: &str) {
    assert!(ProtocolScheme::new(scheme).is_err(), "scheme: {scheme}");
}

#[rstest]
#[case::simple(".acme")]
#[case::multi_char(".tar.gz")]
fn valid_extension(#[case] extension: &str) {
    assert!(
        FileExtension::new(extension).is_ok(),
        "extension: {extension}"
    );
}

#[rstest]
#[case::missing_dot("acme")]
#[case::only_dot(".")]
#[case::forward_slash("./acme")]
#[case::back_slash(".ac\\me")]
#[case::trailing_slash(".acme/")]
fn invalid_extension(#[case] extension: &str) {
    assert!(
        FileExtension::new(extension).is_err(),
        "extension: {extension}"
    );
}
