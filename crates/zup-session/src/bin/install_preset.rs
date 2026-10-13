use zup_preset_ipc::Bootstrap;
use zup_preset_protocol::{Action, Capabilities, InstallerState, Message, Session, SessionId};

fn main() {
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

    peer.frame(Message::Action(Action::Install))
        .map(|frame| channel.sender().send(&frame).expect("ask for an install"))
        .expect("frame the install");

    let mut seen = 0;
    let mut succeeded = false;
    let mut channel = channel;
    loop {
        let (next, incoming) = match channel.recv_within(std::time::Duration::from_secs(10)) {
            Ok(next) => next,
            Err(_) => break,
        };
        channel = next;
        seen += 1;
        if let Message::Snapshot(snapshot) = &incoming.message
            && snapshot.state == InstallerState::Succeeded
        {
            succeeded = true;
        }
        if succeeded || seen > 300 {
            break;
        }
    }
    let _ = channel;
    if !succeeded {
        std::process::exit(3);
    }
}
