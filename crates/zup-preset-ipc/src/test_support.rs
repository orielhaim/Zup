use crate::{Bootstrap, Channel};
use zup_preset_protocol::{Action, Capabilities, InstallerState, Message, Session, SessionId};

pub fn connect() -> (Channel, Session) {
    let channel = Bootstrap::from_arguments(std::env::args().skip(1))
        .expect("a preset is launched with an endpoint")
        .collect()
        .expect("collect the endpoint");

    let session = SessionId(channel.session());
    let mut peer = Session::preset(
        session,
        Capabilities::new(std::iter::empty::<zup_preset_protocol::Capability>()),
        "zup-session-test".into(),
        "0.0.0".into(),
    );
    let hello = peer.opening().expect("open the session");
    channel.sender().send(&hello).expect("greet the host");
    let answer = channel.recv().expect("the host answers the greeting");
    peer.receive(answer).expect("the greeting completes");
    (channel, peer)
}

pub fn send(channel: &Channel, peer: &mut Session, action: Action) {
    let frame = peer
        .frame(Message::Action(action))
        .expect("frame the action");
    channel.sender().send(&frame).expect("the host reads");
}

pub fn close() {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::Close);
}

pub fn install_in(scope: zup_preset_protocol::InstallScope) {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::SetScope { scope });
    send(&channel, &mut peer, Action::Install);
    wait_until_succeeded(channel);
}

pub fn modify_in(scope: zup_preset_protocol::InstallScope) {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::SetScope { scope });
    send(&channel, &mut peer, Action::Modify);
    wait_until_succeeded(channel);
}

pub fn uninstall() {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::RequestUninstall);
    send(&channel, &mut peer, Action::ConfirmUninstall);
    wait_until_succeeded(channel);
}

pub fn install() {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::Install);
    wait_until_succeeded(channel);
}

pub fn install_then_recover() {
    let (mut channel, mut peer) = connect();
    send(&channel, &mut peer, Action::Install);
    loop {
        let (next, incoming) = channel
            .recv_within(std::time::Duration::from_secs(30))
            .expect("the host reports the recovery requirement");
        channel = next;
        let Message::Snapshot(snapshot) = incoming.message else {
            continue;
        };
        if snapshot.state == InstallerState::RecoveryRequired {
            let transaction = snapshot
                .recovery
                .as_ref()
                .map(|target| target.transaction)
                .expect("the engine reported its transaction");
            send(&channel, &mut peer, Action::Recover { transaction });
            break;
        }
        assert_ne!(
            snapshot.state,
            InstallerState::Failed,
            "{:?}",
            snapshot.diagnostic
        );
    }
    wait_until_succeeded(channel);
}

pub fn update() {
    let (mut channel, mut peer) = connect();
    send(&channel, &mut peer, Action::Update);
    loop {
        let (next, incoming) = channel
            .recv_within(std::time::Duration::from_secs(60))
            .expect("the update check completes");
        channel = next;
        let Message::Snapshot(snapshot) = &incoming.message else {
            continue;
        };
        if matches!(
            snapshot.state,
            InstallerState::Failed | InstallerState::RecoveryRequired
        ) {
            panic!("update check failed: {:?}", snapshot.diagnostic);
        }
        if snapshot.update.as_ref().is_some_and(|update| {
            matches!(
                update.state,
                zup_preset_protocol::UpdateState::Available { .. }
            )
        }) {
            send(&channel, &mut peer, Action::Update);
            break;
        }
    }
    wait_until_succeeded(channel);
}

pub fn install_then_fault() {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::Install);
    let mut channel = channel;
    loop {
        let (next, incoming) = channel
            .recv_within(std::time::Duration::from_secs(30))
            .expect("the host publishes while the operation is running");
        channel = next;
        let Message::Snapshot(snapshot) = &incoming.message else {
            continue;
        };
        if snapshot
            .progress
            .as_ref()
            .is_some_and(|progress| progress.label == "holding")
        {
            let _ = channel.sender().send(&zup_preset_protocol::Envelope {
                version: 0,
                session: peer.id(),
                sequence: incoming.sequence.saturating_add(1),
                message: Message::Closed,
            });
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }
    }
}

pub fn install_then_close() {
    let (channel, mut peer) = connect();
    send(&channel, &mut peer, Action::Install);
    let mut channel = channel;
    loop {
        let (next, incoming) = channel
            .recv_within(std::time::Duration::from_secs(30))
            .expect("the host publishes while the operation is running");
        channel = next;
        let Message::Snapshot(snapshot) = &incoming.message else {
            continue;
        };
        if snapshot
            .progress
            .as_ref()
            .is_some_and(|progress| progress.label == "holding")
        {
            send(&channel, &mut peer, Action::Close);
            return;
        }
    }
}

fn wait_until_succeeded(channel: Channel) {
    let mut channel = channel;
    let mut succeeded = false;
    loop {
        let (next, incoming) = match channel.recv_within(std::time::Duration::from_secs(240)) {
            Ok(next) => next,
            Err(_) => break,
        };
        channel = next;
        if let Message::Snapshot(snapshot) = &incoming.message {
            if snapshot.state == InstallerState::Succeeded {
                succeeded = true;
            }
            if matches!(
                snapshot.state,
                InstallerState::Failed | InstallerState::RecoveryRequired
            ) {
                eprintln!(
                    "preset stopped in {:?} {:?}",
                    snapshot.state, snapshot.diagnostic
                );
                std::process::exit(4);
            }
        }
        if succeeded {
            break;
        }
    }
    if !succeeded {
        std::process::exit(3);
    }
}

pub fn recover_journal() {
    let (mut channel, mut peer) = connect();
    let mut expected = None;
    let mut stale_sent = false;
    let mut recovering = false;
    loop {
        let (next, incoming) = channel
            .recv_within(std::time::Duration::from_secs(30))
            .expect("recovery snapshot");
        channel = next;
        let Message::Snapshot(snapshot) = incoming.message else {
            continue;
        };
        match snapshot.state {
            InstallerState::RecoveryRequired => {
                let target = snapshot
                    .recovery
                    .as_ref()
                    .expect("protected recovery target");
                assert_eq!(
                    snapshot.settlement,
                    Some(zup_core::DeploymentSettlement::Committed)
                );
                let id = *expected.get_or_insert(target.transaction);
                assert_eq!(
                    id, target.transaction,
                    "deferral retains the exact transaction"
                );
                if !stale_sent {
                    send(
                        &channel,
                        &mut peer,
                        Action::Recover {
                            transaction: zup_core::TransactionId::new_v7(),
                        },
                    );
                    stale_sent = true;
                }
                send(&channel, &mut peer, Action::Recover { transaction: id });
                recovering = true;
            }
            InstallerState::Succeeded if recovering => {
                assert!(snapshot.recovery.is_none());
                send(&channel, &mut peer, Action::Close);
                return;
            }
            InstallerState::Failed => panic!("recovery failed: {:?}", snapshot.diagnostic),
            _ => {}
        }
    }
}

pub fn recover_then_refresh() {
    let (mut channel, mut peer) = connect();
    let mut requested_refresh = false;
    loop {
        let (next, incoming) = channel
            .recv_within(std::time::Duration::from_secs(30))
            .expect("settlement snapshot");
        channel = next;
        let Message::Snapshot(snapshot) = incoming.message else {
            continue;
        };
        match snapshot.state {
            InstallerState::RecoveryRequired => send(
                &channel,
                &mut peer,
                Action::Recover {
                    transaction: snapshot
                        .recovery
                        .as_ref()
                        .expect("protected recovery projection")
                        .transaction,
                },
            ),
            InstallerState::RefreshRequired => {
                assert_eq!(
                    snapshot.settlement,
                    Some(zup_core::DeploymentSettlement::RolledBack)
                );
                assert!(
                    snapshot.recovery.is_none(),
                    "presentation failure cannot invent a journal"
                );
                assert!(!requested_refresh, "one refresh must complete");
                requested_refresh = true;
                send(&channel, &mut peer, Action::Retry);
            }
            InstallerState::Options | InstallerState::Maintenance if requested_refresh => {
                assert_eq!(
                    snapshot.settlement,
                    Some(zup_core::DeploymentSettlement::RolledBack)
                );
                assert!(snapshot.diagnostic.is_none());
                send(&channel, &mut peer, Action::Close);
                return;
            }
            InstallerState::Failed => panic!("unexpected failure: {:?}", snapshot.diagnostic),
            _ => {}
        }
    }
}
