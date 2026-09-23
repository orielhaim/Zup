//! Template substitution unit tests.

use zup_core::{Template, Variable, VariableValue};

#[test]
fn substitutes_literal_app_variables() {
    let template = Template::parse("${app.id}/${app.name}/${app.version}").unwrap();
    let resolved = template.substitute(|var| match var {
        Variable::AppId => Some(VariableValue::Literal("com.acme".into())),
        Variable::AppName => Some(VariableValue::Literal("Acme".into())),
        Variable::AppVersion => Some(VariableValue::Literal("1.4.0".into())),
        _ => None,
    });
    assert_eq!(resolved.to_string(), "com.acme/Acme/1.4.0");
    assert_eq!(resolved.as_literal(), Some("com.acme/Acme/1.4.0"));
}

#[test]
fn substitutes_install_with_template_and_merges_literals() {
    let install = Template::parse("${known.program_files}/Acme").unwrap();
    let template = Template::parse("${install}/bin").unwrap();
    let resolved = template.substitute(|var| match var {
        Variable::Install => Some(VariableValue::Template(install.clone())),
        _ => None,
    });

    assert_eq!(
        resolved.parts(),
        [
            zup_core::TemplatePart::Variable(Variable::KnownProgramFiles),
            zup_core::TemplatePart::Literal("/Acme/bin".to_owned()),
        ]
    );
    assert_eq!(resolved.to_string(), "${known.program_files}/Acme/bin");
}

#[test]
fn leaves_known_variables_unresolved() {
    let template = Template::parse("${known.local_app_data}/Programs/${app.name}").unwrap();
    let resolved = template.substitute(|var| match var {
        Variable::AppName => Some(VariableValue::Literal("Acme".into())),
        _ => None,
    });
    assert_eq!(
        resolved.to_string(),
        "${known.local_app_data}/Programs/Acme"
    );
}

#[test]
fn adjacent_literals_normalized() {
    let template = Template::parse("a${app.name}b${app.version}c").unwrap();
    let resolved = template.substitute(|var| match var {
        Variable::AppName => Some(VariableValue::Literal("-".into())),
        Variable::AppVersion => Some(VariableValue::Literal("-".into())),
        _ => None,
    });
    assert_eq!(resolved.parts().len(), 1);
    assert_eq!(resolved.to_string(), "a-b-c");
}

#[test]
fn contains_variable_detects_install() {
    assert!(
        Template::parse("${install}/x")
            .unwrap()
            .contains_variable(Variable::Install)
    );
    assert!(
        !Template::parse("${known.desktop}/x")
            .unwrap()
            .contains_variable(Variable::Install)
    );
}
