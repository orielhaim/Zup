//! Unit tests for condition parsing and evaluation.

use std::collections::BTreeSet;

use rstest::rstest;
use zup_core::{ComponentId, Condition, ConditionError};

fn id(value: &str) -> ComponentId {
    ComponentId::new(value).unwrap()
}

fn selected(values: &[&str]) -> BTreeSet<ComponentId> {
    values.iter().map(|value| id(value)).collect()
}

#[test]
fn simple_component() {
    let condition = Condition::parse(r#"component("cli")"#).unwrap();
    assert_eq!(condition, Condition::Component(id("cli")));
    assert!(condition.evaluate(&selected(&["cli"])));
    assert!(!condition.evaluate(&selected(&["core"])));
}

#[test]
fn not_and_or_and_precedence() {
    let condition =
        Condition::parse(r#"component("a") && component("b") || component("c")"#).unwrap();
    assert_eq!(
        condition,
        Condition::Or(
            Box::new(Condition::And(
                Box::new(Condition::Component(id("a"))),
                Box::new(Condition::Component(id("b")))
            )),
            Box::new(Condition::Component(id("c")))
        )
    );

    // && binds tighter than ||
    assert!(condition.evaluate(&selected(&["c"])));
    assert!(condition.evaluate(&selected(&["a", "b"])));
    assert!(!condition.evaluate(&selected(&["a"])));
}

#[test]
fn parentheses_override_precedence() {
    let condition =
        Condition::parse(r#"component("a") && (component("b") || component("c"))"#).unwrap();
    assert!(condition.evaluate(&selected(&["a", "c"])));
    assert!(!condition.evaluate(&selected(&["c"])));
}

#[test]
fn not_binds_tighter_than_and() {
    let condition = Condition::parse(r#"!component("a") && component("b")"#).unwrap();
    assert_eq!(
        condition,
        Condition::And(
            Box::new(Condition::Not(Box::new(Condition::Component(id("a"))))),
            Box::new(Condition::Component(id("b")))
        )
    );
    assert!(condition.evaluate(&selected(&["b"])));
    assert!(!condition.evaluate(&selected(&["a", "b"])));
}

#[test]
fn referenced_components() {
    let condition =
        Condition::parse(r#"!(component("a") || component("b")) && component("c")"#).unwrap();
    let refs = condition.referenced_components();
    assert_eq!(refs, selected(&["a", "b", "c"]), "referenced: {refs:?}");
}

#[test]
fn display_roundtrip() {
    let source = r#"!component("a") && component("b") || component("c")"#;
    let condition = Condition::parse(source).unwrap();
    let reparsed = Condition::parse(&condition.to_string()).unwrap();
    assert_eq!(condition, reparsed);
}

#[rstest]
#[case::empty("")]
#[case::bare_component("component")]
#[case::missing_call("component cli")]
#[case::missing_id(r#"component()"#)]
#[case::missing_close(r#"component(\"cli\""#)]
#[case::unbalanced_paren("(component(\"a\")")]
#[case::double_and("component(\"a\") &&")]
#[case::unknown_fn("foo(\"a\")")]
#[case::stray_text("component(\"a\") leftover")]
fn rejects_malformed(#[case] source: &str) {
    let err = Condition::parse(source).unwrap_err();
    assert!(
        matches!(
            err,
            ConditionError::UnexpectedEnd
                | ConditionError::UnexpectedToken { .. }
                | ConditionError::ExpectedComponentId
        ),
        "source: {source}, err: {err:?}"
    );
}
