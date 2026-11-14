//! Protocol framing and sequence tests.

use zup_core::SessionId;
use zup_protocol::*;

#[test]
fn wrong_version_rejected() {
    let env = WireEnvelope {
        version: PROTOCOL_VERSION + 1,
        session_id: SessionId::new_v7(),
        sequence: 1,
        message: Message::Ping,
    };
    let bytes = serde_json::to_vec(&env).unwrap();
    let err = decode_payload(&bytes).unwrap_err();
    assert!(matches!(err, WireError::VersionMismatch { .. }));
}

#[test]
fn malformed_json_rejected() {
    assert!(decode_payload(b"{not json").is_err());
}

#[test]
fn oversized_frame_rejected() {
    let big = vec![b'x'; MAX_FRAME_BYTES + 1];
    let err = decode_payload(&big).unwrap_err();
    assert!(matches!(err, WireError::FrameTooLarge { .. }));
}

#[test]
fn sequence_monotonic() {
    let mut tracker = SequenceTracker::new();
    tracker.accept(1).unwrap();
    tracker.accept(2).unwrap();
    assert!(matches!(
        tracker.accept(2),
        Err(WireError::DuplicateSequence { .. })
    ));
    assert!(matches!(
        tracker.accept(1),
        Err(WireError::SequenceRegression { .. })
    ));
}

#[test]
fn unknown_completion_and_failure_values_are_protocol_errors() {
    for message in [
        serde_json::json!({"type": "completed", "transaction_id": "01900000-0000-7000-8000-000000000001", "outcome": "future_success"}),
        serde_json::json!({"type": "failed", "kind": "future_retry", "message": "success, reboot, cancelled"}),
    ] {
        let envelope = serde_json::json!({"version": PROTOCOL_VERSION, "session_id": SessionId::new_v7(), "sequence": 1, "message": message});
        assert!(decode_payload(&serde_json::to_vec(&envelope).unwrap()).is_err());
    }
}
