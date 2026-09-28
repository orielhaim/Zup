//! Worker bootstrap, handshake, and security-negative tests.

use zup_bootstrap::{BootstrapId, BootstrapKey, BootstrapPlan, BoundBootstrapPlan};
use zup_core::TargetTriple;
use zup_protocol::{
    ExecuteBootstrap, ExecuteTransaction, Message, PROTOCOL_VERSION, ParentHello, SequenceTracker,
    SessionId, WireEnvelope, WorkerHello, decode_payload, encode_payload,
};
use zup_windows::{
    WorkerBootstrap, WorkerError, WorkerSession, format_bootstrap, parse_bootstrap, pipe_name,
    plan_hash_hex,
};

fn target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn bootstrap() -> WorkerBootstrap {
    WorkerBootstrap {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId::new_v7(),
        pipe_name: pipe_name("abc-123"),
        expected_parent_pid: 42,
        expected_parent_sid: "S-1-5-21-test".into(),
        target: target(),
        expected_plan_hash: "a".repeat(64),
    }
}

/// The only message a worker will act on is an `ExecuteBootstrap` whose target
/// matches the one it was started for. Everything else about the bootstrap
/// string — the plan hash, the session, the pipe — is already bound by the
/// handshake this replays.
#[test]
fn a_bootstrap_is_executed_only_for_the_target_the_worker_started_for() {
    let plan = BootstrapPlan::new(
        BootstrapKey {
            app_id: zup_core::AppId::new("com.example.app").unwrap(),
            app_version: semver::Version::new(1, 0, 0),
            scope: zup_core::SelectedScope::User,
            target: target(),
        },
        Vec::new(),
    )
    .unwrap();
    let bound = BoundBootstrapPlan::new(plan, Default::default()).unwrap();
    let bootstrap_json = serde_json::to_string(&bound).unwrap();
    let hash = plan_hash_hex(&bootstrap_json);

    let opened = || {
        let mut worker_bootstrap = bootstrap();
        worker_bootstrap.expected_plan_hash = hash.clone();
        let session_id = worker_bootstrap.session_id;
        let mut session = WorkerSession::new(worker_bootstrap.clone());
        session
            .handle_message(WireEnvelope {
                version: PROTOCOL_VERSION,
                session_id,
                sequence: 1,
                message: Message::ParentHello(ParentHello {
                    protocol_version: PROTOCOL_VERSION,
                    session_id,
                    target: worker_bootstrap.target.clone(),
                    transaction_id: bound.id.as_uuid(),
                    expected_plan_hash: hash.clone(),
                }),
            })
            .expect("the handshake matches the bootstrap it was given");
        (session, session_id)
    };

    let execute = |target_in_message: TargetTriple| ExecuteBootstrap {
        target: target_in_message,
        bootstrap_json: bootstrap_json.clone(),
        bootstrap_hash: hash.clone(),
        bootstrap_id: BootstrapId::for_plan(&bound.plan).as_uuid(),
        app_id: "com.example.app".into(),
        app_version: "1.0.0".into(),
        scope: "user".into(),
        state_root: r"C:\state".into(),
        quarantine_root: r"C:\quarantine".into(),
        recovery_id: None,
    };
    let envelope = |session_id, message| WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id,
        sequence: 2,
        message,
    };

    let (mut matching, session_id) = opened();
    matching
        .handle_message(envelope(
            session_id,
            Message::ExecuteBootstrap(execute(target())),
        ))
        .expect("the bootstrap the worker was started for is executed");

    let (mut foreign, session_id) = opened();
    assert!(matches!(
        foreign.handle_message(envelope(
            session_id,
            Message::ExecuteBootstrap(execute(
                TargetTriple::parse("arm64-pc-windows-msvc").unwrap()
            ))
        )),
        Err(WorkerError::TargetMismatch)
    ));
}

/// A well-formed bootstrap string with one field replaced, so each negative case
/// below fails at the check it names rather than at the field-count gate.
fn bootstrap_with(field: usize, value: &str) -> String {
    let good = format_bootstrap(&bootstrap());
    let mut parts: Vec<String> = good.split('|').map(str::to_owned).collect();
    parts[field] = value.to_owned();
    parts.join("|")
}

#[test]
fn bootstrap_rejects_malformed() {
    assert!(parse_bootstrap("").is_err());
    assert!(parse_bootstrap("1|sess|pipe|1").is_err());
    assert!(
        parse_bootstrap("not-a-number|00000000-0000-0000-0000-000000000000|p|1|a|x|y").is_err()
    );
    assert!(parse_bootstrap(&bootstrap_with(1, "not-a-uuid")).is_err());
    assert!(parse_bootstrap(&bootstrap_with(2, "")).is_err());
    assert!(parse_bootstrap(&bootstrap_with(2, "bad\\pipe")).is_err());
    assert!(parse_bootstrap(&bootstrap_with(3, "0")).is_err());
    assert!(parse_bootstrap(&bootstrap_with(4, "not-a-sid")).is_err());
    assert!(parse_bootstrap(&bootstrap_with(5, "zz")).is_err());
    assert!(parse_bootstrap(&bootstrap_with(6, "tooshort")).is_err());
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

/// The three properties that make a wire channel safe to run a worker over: the
/// envelope is the worker's own session, it is the protocol both sides speak,
/// and no message is ever delivered twice.
#[test]
fn an_envelope_offers_a_channel_no_replay() {
    let b = bootstrap();
    let mut session = WorkerSession::new(b.clone());

    let envelope = |session_id, version| WireEnvelope {
        version,
        session_id,
        sequence: 0,
        message: Message::Ping,
    };
    assert!(
        session
            .handle_message(envelope(SessionId::new_v7(), PROTOCOL_VERSION))
            .is_err()
    );
    // decode_payload rejects the version before the session ever sees it.
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
            target: target(),
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
fn handshake_rejects_target_mismatch_and_accepts_canonical_aliases() {
    let mut b = bootstrap();
    b.target = TargetTriple::parse("x64-pc-windows-msvc").unwrap();
    let mut session = WorkerSession::new(b.clone());
    let mismatch = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: b.session_id,
        sequence: 1,
        message: Message::ParentHello(ParentHello {
            protocol_version: PROTOCOL_VERSION,
            session_id: b.session_id,
            target: TargetTriple::parse("arm64-pc-windows-msvc").unwrap(),
            transaction_id: uuid::Uuid::nil(),
            expected_plan_hash: b.expected_plan_hash.clone(),
        }),
    };
    assert!(matches!(
        session.handle_message(mismatch),
        Err(WorkerError::TargetMismatch)
    ));

    let mut canonical = bootstrap();
    canonical.target = TargetTriple::parse("x64-pc-windows-msvc").unwrap();
    let parsed = parse_bootstrap(&format_bootstrap(&canonical)).unwrap();
    assert_eq!(
        parsed.target,
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    );
}

#[test]
fn execute_before_auth_rejected() {
    let b = bootstrap();
    let mut session = WorkerSession::new(b.clone());
    let env = WireEnvelope {
        version: PROTOCOL_VERSION,
        session_id: b.session_id,
        sequence: 1,
        message: Message::ExecuteTransaction(Box::new(ExecuteTransaction {
            target: target(),
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
            release: None,
        })),
    };
    assert!(matches!(
        session.handle_message(env),
        Err(WorkerError::AuthFailed(_))
    ));
}

#[test]
fn authenticated_execute_rejects_target_mismatch_before_plan_validation() {
    let b = bootstrap();
    let mut session = WorkerSession::new(b.clone());
    session
        .handle_message(WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: b.session_id,
            sequence: 1,
            message: Message::ParentHello(ParentHello {
                protocol_version: PROTOCOL_VERSION,
                session_id: b.session_id,
                target: b.target.clone(),
                transaction_id: uuid::Uuid::nil(),
                expected_plan_hash: b.expected_plan_hash.clone(),
            }),
        })
        .unwrap();
    let error = session
        .handle_message(WireEnvelope {
            version: PROTOCOL_VERSION,
            session_id: b.session_id,
            sequence: 2,
            message: Message::ExecuteTransaction(Box::new(ExecuteTransaction {
                target: TargetTriple::parse("arm64-pc-windows-msvc").unwrap(),
                plan_json: "not json".into(),
                plan_hash: b.expected_plan_hash.clone(),
                app_id: "com.acme.app".into(),
                app_version: "1.0.0".into(),
                scope: "user".into(),
                payload_root: r"C:\payload".into(),
                payload_overlay_root: None,
                payload_overlay_base_root: None,
                state_root: r"C:\state".into(),
                work_root: r"C:\work".into(),
                recovery_id: None,
                release: None,
            })),
        })
        .unwrap_err();
    assert!(matches!(error, WorkerError::TargetMismatch));
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
            assert!(capabilities.file_transactions_v1);
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert!(zup_windows::worker_capabilities().file_transactions_v1);
        }
        other => panic!("bad hello {other:?}"),
    }
}
