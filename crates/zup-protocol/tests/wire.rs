//! Protocol framing and sequence tests.

use zup_core::TargetTriple;
use zup_protocol::*;

#[test]
fn envelope_roundtrip() {
    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        sequence: 1,
        message: Message::Ping,
    };
    let bytes = encode_payload(&env).unwrap();
    let back = decode_payload(&bytes).unwrap();
    assert_eq!(env, back);
}

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
fn worker_hello_handshake_shape() {
    let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
    let hello = WorkerHello {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        target: target.clone(),
        worker_pid: 1234,
        capabilities: Capabilities {
            file_transactions_v1: true,
            backend_operations_v1: true,
            lifecycle_v1: true,
            prerequisite_bootstrap_v1: true,
        },
    };
    assert!(hello.capabilities.file_transactions_v1);
    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: hello.session_id,
        sequence: 0,
        message: Message::WorkerHello(hello.clone()),
    };
    let bytes = encode_payload(&env).unwrap();
    let back = decode_payload(&bytes).unwrap();
    match back.message {
        Message::WorkerHello(h) => {
            assert_eq!(h.worker_pid, 1234);
            assert_eq!(h.protocol_version, PROTOCOL_VERSION);
            assert_eq!(h.target, target);
        }
        other => panic!("wrong message {other:?}"),
    }
}

#[test]
fn plan_hash_binding_message_roundtrips_with_overlay() {
    assert_eq!(PROTOCOL_VERSION, 1);
    let msg = ExecuteTransaction {
        target: TargetTriple::parse("x64-pc-windows-msvc").unwrap(),
        plan_json: "{}".into(),
        plan_hash: "abc".into(),
        app_id: "com.acme.app".into(),
        app_version: "1.0.0".into(),
        scope: "machine".into(),
        payload_root: r"C:\payload".into(),
        payload_overlay_root: Some(r"C:\state\.zup-payload-overlays\app\user\hash".into()),
        payload_overlay_base_root: Some(r"C:\state".into()),
        state_root: r"C:\state".into(),
        work_root: r"C:\work".into(),
        recovery_id: None,
    };
    let envelope = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        sequence: 1,
        message: Message::ExecuteTransaction(msg.clone()),
    };
    let decoded = decode_payload(&encode_payload(&envelope).unwrap()).unwrap();
    assert_eq!(decoded, envelope);
    let Message::ExecuteTransaction(decoded) = decoded.message else {
        panic!("wrong message");
    };
    assert_eq!(decoded, msg);
    assert!(MAX_PAYLOAD_OVERLAY_PATH_BYTES >= msg.payload_overlay_root.unwrap().len());
}
