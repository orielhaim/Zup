#![cfg(windows)]

use rstest::rstest;
use zup_core::SessionId;
use zup_core::Target;
use zup_protocol::{
    Message, PROTOCOL_VERSION, SequenceTracker, WireEnvelope, decode_payload, encode_payload,
};
use zup_windows::{WorkerBootstrap, WorkerError, format_bootstrap, parse_bootstrap, pipe_name};

fn target() -> Target {
    Target::parse("windows-x64").unwrap()
}

fn bootstrap() -> WorkerBootstrap {
    WorkerBootstrap {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        pipe_name: pipe_name("abc-123"),
        expected_parent_pid: 42,
        expected_parent_sid: "S-1-5-21-test".into(),
        target: target(),
        expected_request_digest: "a".repeat(64),
    }
}

fn bootstrap_with(field: usize, value: &str) -> String {
    let good = format_bootstrap(&bootstrap());
    let mut parts: Vec<String> = good.split('|').map(str::to_owned).collect();
    parts[field] = value.to_owned();
    parts.join("|")
}

#[rstest]
#[case(None, "")]
#[case(None, "1|sess|pipe|1")]
#[case(None, "not-a-number|00000000-0000-0000-0000-000000000000|p|1|a|x|y")]
#[case(Some(1), "not-a-uuid")]
#[case(Some(2), "")]
#[case(Some(2), "bad\\pipe")]
#[case(Some(3), "0")]
#[case(Some(4), "not-a-sid")]
#[case(Some(5), "zz")]
#[case(Some(6), "tooshort")]
fn bootstrap_rejects_malformed(#[case] field: Option<usize>, #[case] value: &str) {
    let input = match field {
        Some(field) => bootstrap_with(field, value),
        None => value.to_owned(),
    };
    assert!(parse_bootstrap(&input).is_err());
}

#[test]
fn bootstrap_rejects_a_foreign_protocol_version() {
    let foreign = bootstrap_with(0, &(PROTOCOL_VERSION + 1).to_string());
    let error = parse_bootstrap(&foreign).expect_err("foreign protocol version");
    assert!(
        matches!(&error, WorkerError::InvalidBootstrap(message) if message.starts_with("protocol version")),
        "{error:?}"
    );
}

#[test]
fn pipe_name_has_no_secrets() {
    let name = pipe_name("session-1234-abcd");
    assert!(name.starts_with("zup-"));
    assert!(!name.contains('@'));
    assert!(name.len() <= 48);
}

#[test]
fn an_envelope_offers_a_channel_no_replay() {
    let b = bootstrap();

    let envelope = |session_id, version| WireEnvelope {
        version,
        session_id,
        sequence: 0,
        message: Message::Ping,
    };
    assert!(
        decode_payload(
            &encode_payload(&envelope(b.session_id, PROTOCOL_VERSION + 1)).unwrap_or_default()
        )
        .is_err()
    );

    let mut tracker = SequenceTracker::new();
    tracker.accept(1).unwrap();
    assert!(tracker.accept(1).is_err(), "a replayed sequence");
    assert!(tracker.accept(0).is_err(), "a sequence that goes backwards");
}

#[test]
fn worker_advertises_required_native_capabilities() {
    let capabilities = zup_windows::worker_capabilities();
    assert!(capabilities.file_transactions_v1);
    assert!(capabilities.backend_operations_v1);
    assert!(capabilities.lifecycle_v1);
}
