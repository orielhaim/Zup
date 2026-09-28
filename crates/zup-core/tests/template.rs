//! Unit tests for template parsing.

use rstest::rstest;
use zup_core::{
    INSTALL_LOCATIONS, InstallLocation, Template, TemplateError, TemplatePart, Variable,
};

/// Anything without a `${...}` is one literal, `$` included.
#[rstest]
#[case::plain("dist/app.exe")]
#[case::bare_dollar("price: $5")]
#[case::empty("")]
fn literal_only(#[case] source: &str) {
    let template = Template::parse(source).unwrap();
    assert_eq!(template.as_literal(), Some(source));
    assert_eq!(template.to_string(), source);
}

#[rstest]
#[case::install("${install}", Variable::Install)]
#[case::app_id("${app.id}", Variable::AppId)]
#[case::app_name("${app.name}", Variable::AppName)]
#[case::app_version("${app.version}", Variable::AppVersion)]
#[case::programs("${location.programs}", Variable::Location(InstallLocation::Programs))]
fn one_variable(#[case] source: &str, #[case] expected: Variable) {
    let template = Template::parse(source).unwrap();
    assert_eq!(
        template.parts(),
        [TemplatePart::Variable(expected)],
        "source: {source}"
    );
    assert_eq!(template.to_string(), source);
}

/// The location names are a wire contract: a template authored against one
/// spelling has to resolve, and every location in the table has to round-trip
/// through its own name.
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
        assert_eq!(InstallLocation::parse(name), Some(location));
    }
    assert_eq!(INSTALL_LOCATIONS.len(), 5, "one entry per location");
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
#[case::unterminated("${install", TemplateError::UnterminatedVariable)]
#[case::empty_name("${}", TemplateError::EmptyVariable)]
#[case::unknown_location("${location.nope}", TemplateError::UnknownVariable { name: "location.nope".into() })]
#[case::empty_location("${location.}", TemplateError::UnknownVariable { name: "location.".into() })]
#[case::unknown_var("${known.foo}", TemplateError::UnknownVariable { name: "known.foo".into() })]
// The `${known.*}` family is retired: every one of those names is now an
// unknown variable, so they are all the same case.
#[case::legacy("${known.program_files}", TemplateError::UnknownVariable { name: "known.program_files".into() })]
#[case::unknown_simple("${nope}", TemplateError::UnknownVariable { name: "nope".into() })]
fn rejects_malformed(#[case] source: &str, #[case] expected: TemplateError) {
    let err = Template::parse(source).unwrap_err();
    assert_eq!(err, expected, "source: {source}");
}
