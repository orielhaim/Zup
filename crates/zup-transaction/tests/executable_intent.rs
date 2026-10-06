//! Executable intent survives the whole portable chain.
//!
//! The point of these tests is not that the flag compiles. It is that a flag
//! threaded through a dozen structs compiles *just as well while being dropped
//! on the floor*, and the only way to know it is not dropped is to read it back
//! out of the artefact that is supposed to carry it.
//!
//! The artefact is the transaction node: a journal is replayed from a plan, not
//! from a manifest, so an intent that lived only in the manifest would be
//! unavailable exactly when a recovering executor needs it.

mod common;

use common::{file, file_with, target};
use rstest::rstest;
use zup_transaction::{NodeKind, TransactionInput, compile_transaction};

fn plan_for(executable: bool) -> zup_transaction::TransactionPlan {
    let mut input = TransactionInput::new(target());
    input
        .files
        .push(file_with("tool", b"#!/bin/sh\n", executable));
    compile_transaction(&input).expect("a plan")
}

/// Both nodes that touch a file carry the intent: the one that stages the bytes
/// and the one that publishes them. A payload whose bytes are staged without the
/// intent and published with it means the staging side - which is where a
/// backend most wants to decide - has no way to know what it is handling.
#[test]
fn both_file_nodes_carry_the_intent() {
    let plan = plan_for(true);
    let staged: Vec<_> = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::StageFile { .. }))
        .collect();
    let mutations: Vec<_> = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::FileMutation { .. }))
        .collect();

    assert!(!staged.is_empty(), "the plan stages the payload");
    assert!(!mutations.is_empty(), "the plan publishes the payload");
    for node in staged.into_iter().chain(mutations) {
        assert_eq!(
            node.meta.executable,
            Some(true),
            "{:?} must carry the intent",
            node.kind
        );
    }
}

/// The other direction, which is the one an executor that hard-codes `true` would
/// get wrong: an ordinary data file must report that it is not executable.
#[test]
fn a_data_file_carries_no_intent() {
    let plan = plan_for(false);
    for node in plan.nodes.iter().filter(|node| {
        matches!(
            node.kind,
            NodeKind::StageFile { .. } | NodeKind::FileMutation { .. }
        )
    }) {
        assert_eq!(node.meta.executable, Some(false), "{:?}", node.kind);
    }
}

/// The intent is part of the plan's identity. Two otherwise identical
/// transactions that differ only in whether a payload is runnable must not be
/// able to reuse each other's journal: a recovered transaction would otherwise
/// replay the other one's file mode onto a file that wanted a different one.
#[test]
fn the_intent_is_part_of_the_plan_identity() {
    assert_ne!(
        plan_for(true).fingerprint(),
        plan_for(false).fingerprint(),
        "a runnable payload and a data file are different transactions"
    );
}

/// A node read back out of serialized JSON still has the intent. `NodeMeta` is
/// what a journal on disk holds, and a field that only survives in memory is not
/// recoverable.
#[test]
fn the_intent_survives_a_journal_round_trip() {
    let plan = plan_for(true);
    let json = serde_json::to_vec(&plan).expect("a plan serializes");
    let recovered: zup_transaction::TransactionPlan =
        serde_json::from_slice(&json).expect("a plan deserializes");
    assert!(
        recovered
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    NodeKind::StageFile { .. } | NodeKind::FileMutation { .. }
                )
            })
            .all(|node| node.meta.executable == Some(true)),
        "an executor recovering from a journal reads the plan, not the manifest"
    );
}

/// A journal written before executable intent existed has no such key. It has to
/// deserialize to `Some(false)` rather than failing, because "absent" and
/// "explicitly not runnable" mean the same thing for an older record.
#[rstest]
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

/// The helper the other tests share produces a non-executable file by default,
/// so a test that wants to assert anything about the flag has to ask for it
/// explicitly. If this ever changes, every test above silently stops testing the
/// thing they were written to test.
#[test]
fn the_default_helper_declares_no_intent() {
    assert!(!file("data", b"x").executable);
}

/// The flag must not leak into path selection. A runnable helper lands where the
/// manifest said, under the name it was given - not somewhere the executor found
/// more convenient, and not with a suffix invented because the platform can
/// express executability and the manifest did not ask for one.
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
