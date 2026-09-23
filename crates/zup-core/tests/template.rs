//! Unit tests for template parsing.

use rstest::rstest;
use zup_core::{Template, TemplateError, TemplatePart, Variable};

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
#[case::program_files("${known.program_files}", Variable::KnownProgramFiles)]
#[case::local_app_data("${known.local_app_data}", Variable::KnownLocalAppData)]
#[case::program_data("${known.program_data}", Variable::KnownProgramData)]
#[case::start_menu("${known.start_menu}", Variable::KnownStartMenu)]
#[case::desktop("${known.desktop}", Variable::KnownDesktop)]
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
fn several_variables_and_literals() {
    let template = Template::parse("${known.local_app_data}/Programs/${app.name}").unwrap();
    assert_eq!(
        template.parts(),
        [
            TemplatePart::Variable(Variable::KnownLocalAppData),
            TemplatePart::Literal("/Programs/".to_owned()),
            TemplatePart::Variable(Variable::AppName),
        ]
    );
    assert_eq!(
        template.to_string(),
        "${known.local_app_data}/Programs/${app.name}"
    );
}

#[rstest]
#[case::unterminated_open("${install", TemplateError::UnterminatedVariable)]
#[case::unterminated_empty_end("${", TemplateError::UnterminatedVariable)]
#[case::empty_name("${}", TemplateError::EmptyVariable)]
#[case::unknown_var("${known.foo}", TemplateError::UnknownVariable { name: "known.foo".into() })]
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
