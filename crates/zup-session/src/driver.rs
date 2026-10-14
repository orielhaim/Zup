use zup_preset_ipc::{Bootstrap, Channel};
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
