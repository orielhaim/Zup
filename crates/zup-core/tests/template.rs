//! Unit tests for template parsing.

use rstest::rstest;
use zup_core::{
    INSTALL_LOCATIONS, InstallLocation, Template, TemplateError, TemplatePart, Variable,
};

#[test]
fn literal_only() {
    let template = Template::parse("dist/app.exe").unwrap();
    assert_eq!(template.as_literal(), Some("dist/app.exe"));
    assert_eq!(template.to_string(), "dist/app.exe");
}

#[test]
fn empty_template() {
    let template = Template::parse("").unwrap();
    assert!(template.is_empty());
    assert_eq!(template.as_literal(), Some(""));
}

#[rstest]
#[case::install("${install}", Variable::Install)]
#[case::app_id("${app.id}", Variable::AppId)]
#[case::app_name("${app.name}", Variable::AppName)]
#[case::app_version("${app.version}", Variable::AppVersion)]
#[case::programs("${location.programs}", Variable::Location(InstallLocation::Programs))]
#[case::user_data("${location.user_data}", Variable::Location(InstallLocation::UserData))]
#[case::shared_data(
    "${location.shared_data}",
    Variable::Location(InstallLocation::SharedData)
)]
#[case::menu("${location.menu}", Variable::Location(InstallLocation::Menu))]
#[case::desktop("${location.desktop}", Variable::Location(InstallLocation::Desktop))]
fn one_variable(#[case] source: &str, #[case] expected: Variable) {
    let template = Template::parse(source).unwrap();
    assert_eq!(
        template.parts(),
        [TemplatePart::Variable(expected)],
        "source: {source}"
    );
    assert_eq!(template.to_string(), source);
}

#[test]
fn install_locations_have_stable_semantic_names() {
    for (location, name) in [
        (InstallLocation::Programs, "programs"),
        (InstallLocation::UserData, "user_data"),
        (InstallLocation::SharedData, "shared_data"),
        (InstallLocation::Menu, "menu"),
        (InstallLocation::Desktop, "desktop"),
    ] {
        assert_eq!(location.as_str(), name);
        assert_eq!(location.to_string(), name);
        assert_eq!(InstallLocation::parse(name), Some(location));
    }
    assert_eq!(INSTALL_LOCATIONS.len(), 5, "one entry per location");
    for location in INSTALL_LOCATIONS {
        assert_eq!(
            InstallLocation::parse(location.as_str()),
            Some(location),
            "location: {location}"
        );
    }
}

#[test]
fn install_locations_serialize_as_their_canonical_name() {
    for location in INSTALL_LOCATIONS {
        let json = serde_json::to_string(&location).unwrap();
        assert_eq!(json, format!("\"{}\"", location.as_str()));
        assert_eq!(
            serde_json::from_str::<InstallLocation>(&json).unwrap(),
            location
        );
    }
    let error = serde_json::from_str::<InstallLocation>("\"nope\"").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown install location `nope`"),
        "error: {error}"
    );
}

#[test]
fn several_variables_and_literals() {
    let template = Template::parse("${location.user_data}/Programs/${app.name}").unwrap();
    assert_eq!(
        template.parts(),
        [
            TemplatePart::Variable(Variable::Location(InstallLocation::UserData)),
            TemplatePart::Literal("/Programs/".to_owned()),
            TemplatePart::Variable(Variable::AppName),
        ]
    );
    assert_eq!(
        template.to_string(),
        "${location.user_data}/Programs/${app.name}"
    );
}

#[rstest]
#[case::unterminated_open("${install", TemplateError::UnterminatedVariable)]
#[case::unterminated_empty_end("${", TemplateError::UnterminatedVariable)]
#[case::empty_name("${}", TemplateError::EmptyVariable)]
#[case::unknown_location("${location.nope}", TemplateError::UnknownVariable { name: "location.nope".into() })]
#[case::empty_location("${location.}", TemplateError::UnknownVariable { name: "location.".into() })]
#[case::unknown_var("${known.foo}", TemplateError::UnknownVariable { name: "known.foo".into() })]
#[case::legacy_program_files("${known.program_files}", TemplateError::UnknownVariable { name: "known.program_files".into() })]
#[case::legacy_local_app_data("${known.local_app_data}", TemplateError::UnknownVariable { name: "known.local_app_data".into() })]
#[case::legacy_program_data("${known.program_data}", TemplateError::UnknownVariable { name: "known.program_data".into() })]
#[case::legacy_start_menu("${known.start_menu}", TemplateError::UnknownVariable { name: "known.start_menu".into() })]
#[case::legacy_desktop("${known.desktop}", TemplateError::UnknownVariable { name: "known.desktop".into() })]
#[case::unknown_simple("${nope}", TemplateError::UnknownVariable { name: "nope".into() })]
fn rejects_malformed(#[case] source: &str, #[case] expected: TemplateError) {
    let err = Template::parse(source).unwrap_err();
    assert_eq!(err, expected, "source: {source}");
}

#[test]
fn dollar_without_brace_is_literal() {
    let template = Template::parse("price: $5").unwrap();
    assert_eq!(template.as_literal(), Some("price: $5"));
}
