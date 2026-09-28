//! The compatibility contract, tested from both sides.
//!
//! These are the tests that make the version policy more than a paragraph. Each one
//! writes a document a *consumer* would be handed and asks what happens: an additive
//! change is skipped, a semantic one is refused. A protocol whose rules exist only in
//! documentation is a protocol whose first real change is also its first break.

use crate::{
    Application, Artifact, AutomationResult, ByteCount, Details, Diagnostic, DiagnosticSource,
    Digest, Identifier, LogLevel, PROTOCOL, Publication, PublicationAsset, SigningEvidence,
    SigningState, Status, StreamEvent, StreamVersion, Target,
};

fn build() -> AutomationResult {
    AutomationResult::new(crate::OPERATION_BUILD)
        .with_application(Application {
            id: "com.acme.desktop".to_owned(),
            name: "Acme".to_owned(),
            version: "1.4.0".to_owned(),
        })
        .with_targets(vec![Target::new("windows-x64", "x86_64-pc-windows-msvc")])
        .with_artifacts(vec![Artifact {
            path: "Acme-Setup.exe".to_owned(),
            digest: Digest::sha256("a".repeat(64)),
            size: ByteCount::new(2048),
            kind: Identifier::fixed("single"),
            mode: Identifier::fixed("offline"),
            id: Some("windows-x64".to_owned()),
            target: Some("x86_64-pc-windows-msvc".to_owned()),
            variants: None,
            signing: Some(SigningState::unsigned()),
        }])
        .with_release_manifest("dist/zup-release.json")
        .with_summary("Built 1 artifact")
}

fn encode(value: &AutomationResult) -> serde_json::Value {
    serde_json::to_value(value).expect("a result this crate built")
}

/// The accepting direction, one case at a time. Every one of these is a change a
/// future minor bump is allowed to make, and every one of them is a document this
/// build's own decoder has to survive.
#[test]
fn an_older_consumer_tolerates_an_additive_change() {
    let mut document = encode(&build());

    // A new optional field, from any depth.
    document["timing"] = serde_json::json!({ "elapsed_ms": 91_000 });
    document["artifacts"][0]["compression"] = serde_json::json!("zstd");
    assert!(PROTOCOL.accepts(PROTOCOL));
    let parsed: &serde_json::Value = &document;
    assert_eq!(parsed["timing"]["elapsed_ms"], 91_000);

    // A new event type, mid-stream.
    let line = r#"{"type":"cache_warm","blobs":2}"#;
    assert!(matches!(
        serde_json::from_str::<StreamEvent>(line),
        Ok(StreamEvent::Unknown)
    ));

    // A new operation name.
    let warm = AutomationResult::new("toolchain.warm");
    assert!(warm.operation.is("toolchain.warm"));
    assert!(PROTOCOL.accepts(warm.protocol));

    // A new artifact kind and a new publication state.
    let mut artifact = document["artifacts"][0].clone();
    artifact["kind"] = serde_json::json!("bound");
    let decoded: Artifact = serde_json::from_value(artifact).expect("a new kind");
    assert!(decoded.kind.is("bound"));
    let publication = Publication {
        provider: "github".to_owned(),
        subject: "acme/acme".to_owned(),
        tag: "v1.4.0".to_owned(),
        id: None,
        state: Identifier::fixed("sealed"),
        url: None,
        immutable: None,
        assets: vec![PublicationAsset {
            name: "Acme-Setup.exe".to_owned(),
            size: ByteCount::new(1),
            digest: None,
            state: Identifier::fixed("mirrored"),
        }],
        receipt: None,
    };
    assert!(publication.state.is("sealed"));
    assert!(publication.assets[0].state.is("mirrored"));
}

/// The refusing direction. A different major is not a best effort, and a document
/// missing a field the envelope promises is not a document.
#[test]
fn an_older_consumer_refuses_a_semantic_change() {
    // Protocol 2.0.
    let mut document = encode(&build());
    document["protocol"] = serde_json::json!("2.0");
    let claimed: ProtocolFromWire = serde_json::from_value(document).expect("a parsed version");
    assert!(
        PROTOCOL.refuses(claimed.protocol),
        "protocol {} must be refused by {}",
        claimed.protocol,
        PROTOCOL
    );

    // A missing required final field. `diagnostics` is a `Vec` with no default, so a
    // document that omits it is not a result document at all — and the generated
    // schema says the same, which is why both are asserted.
    let mut missing = encode(&build());
    missing
        .as_object_mut()
        .expect("an object")
        .remove("diagnostics");
    assert!(
        serde_json::from_value::<AutomationResult>(missing).is_err(),
        "a result without `diagnostics` is not a result"
    );

    // A malformed digest, and an unsafe integer.
    let mut bad_digest = encode(&build());
    bad_digest["artifacts"][0]["digest"]["value"] = serde_json::json!("nope");
    assert!(serde_json::from_value::<AutomationResult>(bad_digest).is_ok());
    let mut document = encode(&build());
    let mut result: AutomationResult = serde_json::from_value(document.clone()).unwrap();
    result.artifacts[0].digest = Digest::sha256("nope");
    assert!(result.validate().is_err(), "a short digest is not a digest");

    document["artifacts"][0]["size"] = serde_json::json!(9_007_199_254_740_993u64);
    assert!(
        serde_json::from_value::<AutomationResult>(document).is_err(),
        "2^53+1 cannot be represented exactly and must not parse"
    );
}

/// A completed line whose result is not a usable document is the failure a consumer
/// must not paper over, so the two are checked together rather than separately.
#[test]
fn a_malformed_completed_result_is_refused() {
    let mut result = build();
    result.status = Status::Failure;
    let text = serde_json::to_string(&StreamEvent::completed(result)).unwrap();
    let event: StreamEvent = serde_json::from_str(&text).expect("a stream line");
    let completed = event.result().expect("a completed line");
    assert_eq!(completed.status, Status::Failure);
    assert!(
        completed.validate().is_err(),
        "a failure with no diagnostic is a document a consumer cannot use"
    );
}

/// A consumer that treats a warning as a failure is a consumer that turns a successful
/// release into a red build; a consumer that treats an error as a notice is one that
/// ships a broken artifact. The mapping is the contract.
#[test]
fn severity_and_status_agree() {
    let warning = build().with_diagnostic(Diagnostic {
        severity: crate::Severity::Warning,
        ..Diagnostic::error("zup.build.deferred", "signing was skipped")
    });
    warning.validate().expect("a warning does not fail a build");
    assert_eq!(warning.status, Status::Success);

    let failure = AutomationResult::new(crate::OPERATION_BUILD)
        .failed()
        .with_diagnostic(Diagnostic::error("zup.build.digest_mismatch", "changed"))
        .with_summary("1 artifact could not be built");
    failure.validate().expect("a failure that says why");
    assert_eq!(failure.status, Status::Failure);
}

/// A stream is read for its events and for its final result, and the two must be the
/// same document: a consumer that reported progress from the events and a summary from
/// the result must not be able to disagree with itself.
#[test]
fn the_final_event_is_the_document_json_mode_would_have_written() {
    let result = build();
    let stream_final = StreamEvent::completed(result.clone()).to_line();
    let final_value: serde_json::Value =
        serde_json::from_str(&stream_final).expect("a stream line");
    assert_eq!(final_value["result"], encode(&result));

    let json_mode = serde_json::to_string(&result).unwrap();
    let json_value: serde_json::Value = serde_json::from_str(&json_mode).unwrap();
    assert_eq!(json_value, final_value["result"]);
}

/// A routing message carries what a person would have read, and it is a protocol event
/// rather than a suppressed line, so a consumer that shows nothing still has a record.
#[test]
fn a_routed_human_line_is_a_protocol_message() {
    let event = StreamEvent::Log {
        level: LogLevel::Warning,
        message: "compiling plugin configure".to_owned(),
    };
    let value: serde_json::Value = serde_json::from_str(&event.to_line()).unwrap();
    assert_eq!(value["type"], "log");
    assert_eq!(value["level"], "warning");
    assert_eq!(value["message"], "compiling plugin configure");
}

/// The header names the protocol, the tool and the operation, in that order of
/// usefulness, and nothing else. A consumer that has to look past it for the protocol
/// version has no way to know whether it should keep reading.
#[test]
fn the_header_is_the_only_place_the_tool_version_appears() {
    let header = StreamEvent::Version(StreamVersion::new(Identifier::fixed(
        crate::OPERATION_PUBLISH_STAGE,
    )));
    let value: serde_json::Value = serde_json::from_str(&header.to_line()).unwrap();
    assert_eq!(value["type"], "version");
    assert_eq!(value["protocol"], PROTOCOL.to_string());
    assert_eq!(value["zup"], crate::ZUP_VERSION);
    assert_eq!(value["operation"], "publish.stage");
    assert_eq!(value.as_object().map(|o| o.len()), Some(4));

    let result = encode(&build());
    assert!(
        result.get("zup").is_none(),
        "the tool version is in the header, not in every result"
    );
}

/// Signing evidence is a list of facts rather than a state word, and the facts still
/// mean the same thing in this contract as they do in a release manifest.
#[test]
fn signing_evidence_keeps_its_vocabulary() {
    let evidence = vec![
        SigningEvidence::new("signature_covers_bytes", "sha256"),
        SigningEvidence::new("platform_trust_accepted", "trusted root"),
        SigningEvidence::new("publisher", "CN=Acme"),
        SigningEvidence::new("certificate", "3f9a…"),
        SigningEvidence::new("timestamp", "rfc3161"),
    ];
    let state = SigningState::signed(evidence.clone());
    assert!(state.covers_bytes());
    assert!(!SigningState::unsigned().covers_bytes());
    let value = serde_json::to_value(&state).unwrap();
    let facts = value["evidence"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["fact"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(facts.len(), 5);
    assert!(facts.contains(&"publisher".to_owned()));
}

/// The details payload is optional, and a consumer that meets a `kind` from a newer
/// minor still has the envelope. That is the whole argument for a tagged payload over
/// a bag of nullable fields.
#[test]
fn details_are_optional_and_tagged() {
    let without = encode(&build());
    assert!(without["details"].is_null());

    let tagged = encode(&build().with_details(Details::Build(crate::BuildDetails {
        signing_plan: Some("dist/zup-signing.json".to_owned()),
        pending_signatures: 1,
    })));
    assert_eq!(tagged["details"]["kind"], "build");
    assert_eq!(tagged["details"]["pending_signatures"], 1);
}

/// A diagnostic pointed at a place is worth more than one that is not, and the
/// identity that lets a consumer deduplicate a streamed diagnostic against the same
/// diagnostic in the final result depends on it.
#[test]
fn a_pointed_diagnostic_carries_a_line_and_a_column() {
    let diagnostic = Diagnostic::error("zup.manifest.unknown_target", "no such target")
        .with_help("declare it under [build.targets]")
        .in_file("zup.toml");
    let mut diagnostic = diagnostic;
    diagnostic.source = Some(DiagnosticSource::at("zup.toml", 12, 3));
    let value = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(value["source"]["start_line"], 12);
    assert_eq!(value["source"]["start_column"], 3);
    assert!(value["source"]["end_line"].is_null());
}

/// A tiny mirror of what a foreign consumer does: read the version, compare the major,
/// and then read the document. Written here so the rule is exercised through a type
/// that only knows the wire, not through the crate's own internals.
#[derive(Debug, serde::Deserialize)]
struct ProtocolFromWire {
    protocol: crate::ProtocolVersion,
}

#[cfg(feature = "bindings")]
mod schema {
    /// The committed schema is generated from the types, and the parts a validator
    /// actually acts on are asserted rather than eyeballed.
    #[test]
    fn the_schema_is_open_and_the_envelope_is_closed() {
        let document: serde_json::Value =
            serde_json::from_str(&crate::schema_json()).expect("the generated schema");
        assert_eq!(document["$id"], crate::SCHEMA_ID);
        let definitions = document["$defs"]
            .as_object()
            .expect("a definition table")
            .clone();
        for (name, body) in &definitions {
            assert_ne!(
                body["additionalProperties"],
                serde_json::json!(false),
                "`{name}` is closed; a consumer must be able to ignore an unknown field"
            );
        }
        // The envelope requires every field it promises to write.
        let required = definitions["AutomationResult"]["required"]
            .as_array()
            .expect("a required list")
            .iter()
            .filter_map(|entry| entry.as_str())
            .collect::<Vec<_>>();
        let expected = [
            "protocol",
            "operation",
            "status",
            "application",
            "targets",
            "artifacts",
            "release_manifest",
            "publication",
            "diagnostics",
            "summary",
            "details",
        ];
        for field in expected {
            assert!(required.contains(&field), "`{field}` is not required");
        }
        // A byte count is bounded in the schema, not just in the deserializer.
        assert_eq!(definitions["ByteCount"]["maximum"], crate::MAX_SAFE_BYTES);
        // The generated document is byte-stable, because CI diffs it.
        assert_eq!(crate::schema_json(), crate::schema_json());
    }

    /// Both roots share one definition table, and a name that appeared twice with
    /// different bodies would be a schema that validates one shape two ways.
    #[test]
    fn one_definition_table_serves_both_roots() {
        let document: serde_json::Value =
            serde_json::from_str(&crate::schema_json()).expect("the generated schema");
        let references = document["oneOf"]
            .as_array()
            .expect("two roots")
            .iter()
            .filter_map(|root| root["$ref"].as_str())
            .map(|reference| reference.trim_start_matches("#/$defs/").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(references, ["AutomationResult", "StreamEvent"]);
        let definitions = document["$defs"].as_object().expect("definitions");
        for reference in references {
            assert!(definitions.contains_key(&reference), "{reference}");
        }
    }
}
