use rstest::rstest;
use zup_preset_protocol::{
    Capabilities, Capability, DesktopEnvironment, GraphicalSession, HostEnvironment, HostHello,
    MachineAuthorization, OperatingSystem, ProductIdentity, Session, SessionId, SessionProgress,
};

fn environment() -> HostEnvironment {
    HostEnvironment {
        operating_system: OperatingSystem::Linux,
        architecture: "x86_64".into(),
        target: "linux-x64".into(),
        desktop: DesktopEnvironment {
            session: GraphicalSession::Wayland,
            desktop_id: Some("gnome".into()),
            desktop_name: None,
        },
        machine_authorization: MachineAuthorization::Available,
    }
}

fn product() -> ProductIdentity {
    ProductIdentity {
        name: "Acme".into(),
        publisher: None,
        version: "1.4.0".into(),
        description: None,
    }
}

#[test]
fn the_environment_survives_the_wire() {
    let hello = HostHello::new(
        SessionId::new_v7(),
        Capabilities::new([Capability::PlanPreview]),
        product(),
        "0.8.4",
        environment(),
    );
    let bytes = serde_json::to_vec(&hello).expect("encode");
    let decoded: HostHello = serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(decoded, hello);
    assert!(decoded.environment.graphical());
}

#[rstest]
#[case(MachineAuthorization::Available, true)]
#[case(MachineAuthorization::Elevated, true)]
#[case(
    MachineAuthorization::Unavailable { reason: "no polkit".into() },
    false
)]
fn machine_authorization_says_whether_machine_scope_can_be_offered(
    #[case] authorization: MachineAuthorization,
    #[case] expected: bool,
) {
    assert_eq!(authorization.can_authorize(), expected);
}

#[rstest]
#[case(GraphicalSession::Wayland, true)]
#[case(GraphicalSession::X11, true)]
#[case(GraphicalSession::Native, true)]
#[case(GraphicalSession::None, false)]
#[case(GraphicalSession::Unknown, false)]
fn a_desktop_reports_whether_a_window_can_be_shown(
    #[case] session: GraphicalSession,
    #[case] expected: bool,
) {
    let desktop = DesktopEnvironment {
        session,
        desktop_id: None,
        desktop_name: None,
    };
    assert_eq!(desktop.graphical(), expected);
}

#[test]
fn the_preset_learns_the_environment_in_the_handshake() {
    let id = SessionId::new_v7();
    let mut host = Session::host(
        id,
        Capabilities::new([Capability::PlanPreview]),
        product(),
        environment(),
    );
    let mut preset = Session::preset(
        id,
        Capabilities::new([Capability::PlanPreview]),
        "aurora".into(),
        "1.0.0".into(),
    );
    let hello = preset.opening().expect("the preset opens");
    let SessionProgress::Send(answer) = host.receive(hello).expect("the host answers") else {
        panic!("the host answers a hello with its own hello");
    };
    assert!(preset.receive(answer).is_ok());
    assert_eq!(
        preset.environment(),
        Some(&environment()),
        "the handshake carries the host environment, not just capabilities"
    );
}

#[test]
fn an_environment_names_its_target() {
    let detected = zup_preset_protocol::environment::current("linux-x64");
    assert_eq!(detected.target, "linux-x64");
    assert_eq!(detected.architecture, std::env::consts::ARCH);
}
