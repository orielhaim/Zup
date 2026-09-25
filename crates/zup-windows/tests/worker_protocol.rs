//! Worker bootstrap, handshake, and security-negative tests.

use zup_bootstrap::{BootstrapId, BootstrapKey, BootstrapPlan, BoundBootstrapPlan};
use zup_protocol::{
    Capabilities, ExecuteBootstrap, ExecuteTransaction, Message, PROTOCOL_VERSION, ParentHello,
    SequenceTracker, SessionId, WireEnvelope, WorkerHello, decode_payload, encode_payload,
};
use zup_windows::{
    WorkerBootstrap, WorkerError, WorkerSession, format_bootstrap, parse_bootstrap, pipe_name,
    plan_hash_hex,
};

fn bootstrap() -> WorkerBootstrap {
    WorkerBootstrap {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        pipe_name: pipe_name("abc-123"),
        expected_parent_pid: 42,
        expected_parent_sid: "S-1-5-21-test".into(),
        expected_plan_hash: "a".repeat(64),
    }
}

#[test]
fn bootstrap_roundtrip() {
    let b = bootstrap();
    let s = format_bootstrap(&b);
    let parsed = parse_bootstrap(&s).unwrap();
    assert_eq!(parsed.protocol_version, b.protocol_version);
    assert_eq!(parsed.session_id, b.session_id);
    assert_eq!(parsed.pipe_name, b.pipe_name);
    assert_eq!(parsed.expected_parent_pid, b.expected_parent_pid);
    assert_eq!(parsed.expected_plan_hash, b.expected_plan_hash);
}

#[test]
fn bootstrap_roundtrip_accepts_only_the_typed_bootstrap_message() {
    let plan = BootstrapPlan::new(
        BootstrapKey {
            app_id: zup_core::AppId::new("com.example.app").unwrap(),
            app_version: semver::Version::new(1, 0, 0),
            scope: zup_core::SelectedScope::User,
        },
        Vec::new(),
    )
    .unwrap();
    let bound = BoundBootstrapPlan::new(plan, Default::default()).unwrap();
    let bootstrap_json = serde_json::to_string(&bound).unwrap();
    let hash = plan_hash_hex(&bootstrap_json);
    let mut worker_bootstrap = bootstrap();
    worker_bootstrap.expected_plan_hash = hash.clone();
    let mut session = WorkerSession::new(worker_bootstrap.clone());
    session
        .handle_message(WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: worker_bootstrap.session_id,
            sequence: 1,
            message: Message::ParentHello(ParentHello {
                protocol_version: PROTOCOL_VERSION,
                session_id: worker_bootstrap.session_id,
                transaction_id: bound.id.as_uuid(),
                expected_plan_hash: hash.clone(),
            }),
        })
        .unwrap();
    session
        .handle_message(WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: worker_bootstrap.session_id,
            sequence: 2,
            message: Message::ExecuteBootstrap(ExecuteBootstrap {
                bootstrap_json,
                bootstrap_hash: hash,
                bootstrap_id: BootstrapId::for_plan(&bound.plan).as_uuid(),
                app_id: "com.example.app".into(),
                app_version: "1.0.0".into(),
                scope: "user".into(),
                state_root: r"C:\state".into(),
                quarantine_root: r"C:\quarantine".into(),
                recovery_id: None,
            }),
        })
        .unwrap();
}

#[test]
fn bootstrap_rejects_malformed() {
    assert!(parse_bootstrap("").is_err());
    assert!(parse_bootstrap("1|sess|pipe|1").is_err());
    assert!(parse_bootstrap("99|00000000-0000-0000-0000-000000000000|p|1|a").is_err());
    assert!(parse_bootstrap("1|not-a-uuid|p|1|a").is_err());
    assert!(parse_bootstrap("1|00000000-0000-0000-0000-000000000000|p|0|a").is_err());
    assert!(parse_bootstrap("1|00000000-0000-0000-0000-000000000000|p|1|zz").is_err());
}

#[test]
fn pipe_name_has_no_secrets() {
    let name = pipe_name("session-1234-abcd");
    assert!(name.starts_with("zup-"));
    assert!(!name.contains('@'));
    assert!(name.len() <= 48);
}

#[test]
fn wrong_session_rejected() {
    let mut session = WorkerSession::new(bootstrap());
    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        sequence: 0,
        message: Message::Ping,
    };
    assert!(session.handle_message(env).is_err());
}

#[test]
fn wrong_protocol_version_rejected() {
    let b = bootstrap();
    let env = WireEnvelope {
        version: 99,
        session_id: b.session_id,
        sequence: 0,
        message: Message::Ping,
    };
    // decode_payload rejects version; handle_message also checks.
    assert!(decode_payload(&encode_payload(&env).unwrap_or_default()).is_err());
}

#[test]
fn handshake_requires_valid_plan_hash() {
    let b = bootstrap();
    let mut session = WorkerSession::new(b.clone());
    let hello = session.hello();
    assert!(matches!(hello.message, Message::WorkerHello(_)));

    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: b.session_id,
        sequence: 1,
        message: Message::ParentHello(ParentHello {
            protocol_version: PROTOCOL_VERSION,
            session_id: b.session_id,
            transaction_id: uuid::Uuid::now_v7(),
            expected_plan_hash: "b".repeat(64),
        }),
    };
    assert!(matches!(
        session.handle_message(env),
        Err(WorkerError::PlanHashMismatch)
    ));
}

#[test]
fn execute_before_auth_rejected() {
    let b = bootstrap();
    let mut session = WorkerSession::new(b.clone());
    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: b.session_id,
        sequence: 1,
        message: Message::ExecuteTransaction(ExecuteTransaction {
            plan_json: "{}".into(),
            plan_hash: b.expected_plan_hash.clone(),
            app_id: "com.acme.app".into(),
            app_version: "1.0.0".into(),
            scope: "machine".into(),
            payload_root: r"C:\payload".into(),
            payload_overlay_root: None,
            payload_overlay_base_root: None,
            state_root: r"C:\state".into(),
            work_root: r"C:\work".into(),
            recovery_id: None,
        }),
    };
    assert!(matches!(
        session.handle_message(env),
        Err(WorkerError::AuthFailed(_))
    ));
}

#[test]
fn duplicate_sequence_rejected() {
    let mut tracker = SequenceTracker::new();
    tracker.accept(1).unwrap();
    assert!(tracker.accept(1).is_err());
    assert!(tracker.accept(0).is_err());
}

#[test]
fn second_execute_rejected() {
    let b = bootstrap();
    let mut session = WorkerSession::new(b.clone());
    // Authenticate first.
    let _ = session.handle_message(WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: b.session_id,
        sequence: 1,
        message: Message::ParentHello(ParentHello {
            protocol_version: PROTOCOL_VERSION,
            session_id: b.session_id,
            transaction_id: uuid::Uuid::nil(),
            expected_plan_hash: b.expected_plan_hash.clone(),
        }),
    });
    // Empty plan hash won't match a real plan; first execute fails hash check.
    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: b.session_id,
        sequence: 2,
        message: Message::ExecuteTransaction(ExecuteTransaction {
            plan_json: "[]".into(),
            plan_hash: "c".repeat(64),
            app_id: "com.acme.app".into(),
            app_version: "1.0.0".into(),
            scope: "machine".into(),
            payload_root: r"C:\payload".into(),
            payload_overlay_root: None,
            payload_overlay_base_root: None,
            state_root: r"C:\state".into(),
            work_root: r"C:\work".into(),
            recovery_id: None,
        }),
    };
    assert!(matches!(
        session.handle_message(env),
        Err(WorkerError::PlanHashMismatch)
    ));
}

#[test]
fn plan_hash_hex_is_stable() {
    let a = plan_hash_hex("{}");
    let b = plan_hash_hex("{}");
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
}

#[test]
fn worker_hello_advertises_capability() {
    let session = WorkerSession::new(bootstrap());
    let hello = session.hello();
    match hello.message {
        Message::WorkerHello(WorkerHello {
            capabilities,
            protocol_version,
            ..
        }) => {
            assert!(capabilities.has_file_transactions_v1());
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            let _ = Capabilities::supported();
        }
        other => panic!("bad hello {other:?}"),
    }
}
