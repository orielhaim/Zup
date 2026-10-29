mod common;

use common::{file_with, target};
use zup_transaction::{NodeKind, TransactionInput, compile_transaction};

fn plan_for(executable: bool) -> zup_transaction::TransactionPlan {
    let mut input = TransactionInput::new(target());
    input
        .files
        .push(file_with("tool", b"#!/bin/sh\n", executable));
    compile_transaction(&input).expect("a plan")
}

#[rstest::rstest]
#[case::executable(true)]
#[case::data(false)]
fn file_intent_survives_transaction_compilation_and_journaling(#[case] executable: bool) {
    let plan = plan_for(executable);
    let recovered: zup_transaction::TransactionPlan =
        serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
    for plan in [&plan, &recovered] {
        let staged = plan
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::StageFile { .. }))
            .collect::<Vec<_>>();
        let mutations = plan
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
            .collect::<Vec<_>>();
        assert!(!staged.is_empty());
        assert!(!mutations.is_empty());
        for node in staged.into_iter().chain(mutations) {
            assert_eq!(node.meta.executable, Some(executable));
        }
    }
}

#[test]
fn the_intent_is_part_of_the_plan_identity() {
    assert_ne!(
        plan_for(true).fingerprint(),
        plan_for(false).fingerprint(),
        "a runnable payload and a data file are different transactions"
    );
}

#[test]
fn an_older_journal_without_the_key_is_still_readable() {
    let plan = plan_for(true);
    let json: serde_json::Value = serde_json::to_value(&plan).expect("a plan serializes");
    let mut json = json;
    for node in json["nodes"].as_array_mut().expect("nodes") {
        node["meta"]
            .as_object_mut()
            .expect("meta")
            .remove("executable");
    }
    let recovered: zup_transaction::TransactionPlan =
        serde_json::from_value(json).expect("a plan without the key deserializes");
    for node in recovered.nodes.iter().filter(|node| {
        matches!(
            node.kind,
            NodeKind::StageFile { .. } | NodeKind::FileMutation { .. }
        )
    }) {
        assert_eq!(
            node.meta.executable, None,
            "an absent key stays absent rather than being invented"
        );
    }
}

#[test]
fn the_intent_does_not_change_where_a_file_lands() {
    let destination_of = |plan: &zup_transaction::TransactionPlan| {
        plan.nodes
            .iter()
            .find_map(|node| match &node.kind {
                NodeKind::FileMutation {
                    key: zup_core::ResourceKey::File { destination },
                    ..
                } => Some(destination.clone()),
                _ => None,
            })
            .expect("a file mutation")
    };
    assert_eq!(
        destination_of(&plan_for(true)),
        destination_of(&plan_for(false))
    );
    assert!(
        !destination_of(&plan_for(true)).ends_with(".exe"),
        "the flag must not invent a suffix the manifest did not ask for"
    );
}
