//! Protocol framing and sequence tests.

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
