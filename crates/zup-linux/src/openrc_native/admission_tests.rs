struct AdmissionFixture {
    manager: Openrc,
    key: zup_core::ResourceKey,
    name: String,
    root: PathBuf,
    state: ServiceState,
}

impl AdmissionFixture {
    fn new(supervised: bool, start: ServiceStart) -> Self {
        assert!(std::env::var_os("ZUP_TEST_STOCK_OPENRC").is_some());
        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let tag = uuid::Uuid::now_v7().simple().to_string();
        let name = format!("zup-admission-{tag}.instance");
        let root = Path::new("/opt").join(&name);
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = root.join("sleep");
        std::fs::copy("/bin/busybox", &executable).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = ServiceState::Registration {
            state_directories: vec![],
            native_timeouts: zup_core::NativeServiceTimeouts::Openrc {
                stop_grace_seconds: 1,
                kill_wait_seconds: 2,
            },
            execution: ServiceExecution::OpenrcUser {
                user: "nobody".into(),
                group: Some("nobody".into()),
            },
            failure: if supervised {
                ServiceFailurePolicy::Openrc {
                    delay_seconds: 1,
                    max_respawns: 3,
                    respawn_period_seconds: 60,
                }
            } else {
                ServiceFailurePolicy::None
            },
            dependencies: vec![],
            display_name: "Admission fixture".into(),
            command: Box::new(CommandSpec::new(
                TargetPath::new(Target::host().unwrap(), executable.to_string_lossy()).unwrap(),
                vec!["600".into()],
            )),
            start,
            readiness: zup_core::ServiceReadiness::Manager,
        };
        let key = ResourceKey::Service {
            id: ServiceId::new(&name).unwrap(),
        };
        let manager = Openrc::connect(crate::locations::MachineRoots::production()).unwrap();
        assert_eq!(
            manager.observe_registration(&name).unwrap(),
            ObservedServiceState::Absent
        );
        OwnedDirectory::open(Path::new("/etc/init.d"))
            .unwrap()
            .create_durable_exclusive(
                &name,
                &crate::openrc::render(&name, &state).unwrap(),
                rustix::fs::Mode::from_bits_truncate(0o755),
            )
            .unwrap();
        let registration = declaration(&key, &name, &state, None);
        let receipt = manager.apply_registration(&registration).unwrap();
        manager
            .verify_registration(&registration, &receipt)
            .unwrap();
        Self {
            manager,
            key,
            name,
            root,
            state,
        }
    }

    fn state(&self, start: ServiceStart) -> ServiceState {
        let mut state = self.state.clone();
        if let ServiceState::Registration { start: policy, .. } = &mut state {
            *policy = start;
        }
        state
    }

    fn assert_observed(&self, start: ServiceStart, runtime: ServiceRuntimeState) {
        let observed = self.manager.observe_registration(&self.name).unwrap();
        assert_eq!(observed.configuration(), Some(self.state(start)));
        assert!(
            matches!(observed, ObservedServiceState::Service { runtime_state: Some(actual), .. } if actual == runtime)
        );
    }

    fn assert_blocked(&self) {
        for action in ["start", "restart"] {
            assert!(
                self.manager
                    .command(
                        "/sbin/rc-service",
                        &["--nodeps", &self.name, action],
                        Duration::from_secs(10)
                    )
                    .is_err()
            );
        }
        let path = Openrc::source(&self.name).unwrap();
        assert!(
            self.manager
                .command(path.to_str().unwrap(), &["start"], Duration::from_secs(10))
                .is_err()
        );
        assert!(
            self.manager
                .command(
                    "/sbin/openrc-run",
                    &[path.to_str().unwrap(), "start"],
                    Duration::from_secs(10)
                )
                .is_err()
        );
        assert!(!ProcessScope::observe(&self.name).unwrap().populated);
    }

    fn dispose(self) {
        self.manager
            .set_startup(&self.name, &self.state(ServiceStart::Disabled))
            .unwrap();
        self.manager
            .command(
                "/sbin/rc-service",
                &[&self.name, "stop"],
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(!ProcessScope::observe(&self.name).unwrap().populated);
        let source = OwnedDirectory::open(Path::new("/etc/init.d")).unwrap();
        source.remove_file(&self.name).unwrap();
        source.sync().unwrap();
        self.manager
            .set_startup(&self.name, &ServiceState::Absent)
            .unwrap();
        self.manager
            .set_startup(&self.name, &ServiceState::Absent)
            .unwrap();
        assert_eq!(
            self.manager.observe_registration(&self.name).unwrap(),
            ObservedServiceState::Absent
        );
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

#[rstest::rstest]
#[case::direct(false)]
#[case::supervised(true)]
#[ignore = "requires a disposable stock OpenRC cgroup-v2 VM as root"]
fn stock_admission_policies_and_configuration(#[case] supervised: bool) {
    let fixture = AdmissionFixture::new(supervised, ServiceStart::Manual);
    let configuration = OwnedDirectory::open(Path::new("/etc/conf.d")).unwrap();
    let base = fixture.name.split('.').next().unwrap();
    let overrides = [
        format!("{}.default", fixture.name),
        format!("{base}.default"),
    ];
    for name in &overrides {
        configuration
            .create_durable_exclusive(name, b"", rustix::fs::Mode::from_bits_truncate(0o644))
            .unwrap();
    }
    for policy in [
        ServiceStart::Automatic,
        ServiceStart::Manual,
        ServiceStart::Disabled,
    ] {
        for _ in 0..2 {
            fixture
                .manager
                .set_startup(&fixture.name, &fixture.state(policy))
                .unwrap();
        }
        fixture.assert_observed(policy, ServiceRuntimeState::Stopped);
        assert_eq!(
            fixture.manager.runlevel_policy(&fixture.name).unwrap(),
            if policy == ServiceStart::Automatic {
                RunlevelPolicy::Automatic
            } else {
                RunlevelPolicy::Manual
            }
        );
        if policy == ServiceStart::Disabled {
            fixture.assert_blocked();
        } else {
            for action in ["start", "restart"] {
                fixture
                    .manager
                    .command(
                        "/sbin/rc-service",
                        &[&fixture.name, action],
                        Duration::from_secs(10),
                    )
                    .unwrap();
                fixture.assert_observed(policy, ServiceRuntimeState::Running);
            }
            fixture
                .manager
                .command(
                    "/sbin/rc-service",
                    &[&fixture.name, "stop"],
                    Duration::from_secs(10),
                )
                .unwrap();
        }
    }
    let override_bytes = b"zup_disabled=no\ncommand=/bin/false\ncommand_background=no\nsupervisor=s6\nrc_cgroup_mode=legacy\n";
    configuration
        .write_durable(
            &overrides[0],
            override_bytes,
            rustix::fs::Mode::from_bits_truncate(0o644),
        )
        .unwrap();
    assert!(fixture.manager.observe_registration(&fixture.name).is_err());
    fixture.assert_blocked();
    assert_eq!(
        configuration.read_regular(&overrides[0]).unwrap(),
        override_bytes
    );
    configuration
        .write_durable(
            &overrides[0],
            b"",
            rustix::fs::Mode::from_bits_truncate(0o666),
        )
        .unwrap();
    assert!(fixture.manager.observe_registration(&fixture.name).is_err());
    configuration.remove_file(&overrides[0]).unwrap();
    rustix::fs::symlinkat("/dev/null", configuration.as_fd(), overrides[0].as_str()).unwrap();
    assert!(fixture.manager.observe_registration(&fixture.name).is_err());
    configuration.remove_file(&overrides[0]).unwrap();
    let admission = fixture.manager.admission_directory(false).unwrap().unwrap();
    for bytes in [&b"inhibited\nextra\n"[..], &b"unknown\n"[..]] {
        admission
            .write_durable(&fixture.name, bytes, crate::fs::STATE_FILE_MODE)
            .unwrap();
        assert!(fixture.manager.admission(&fixture.name).is_err());
        fixture.assert_blocked();
    }
    admission.remove_file(&fixture.name).unwrap();
    assert!(
        fixture
            .manager
            .observe_registration(&fixture.name)
            .unwrap()
            .configuration()
            .is_none()
    );
    fixture.assert_blocked();
    fixture
        .manager
        .set_startup(&fixture.name, &fixture.state(ServiceStart::Disabled))
        .unwrap();
    admission
        .chmod_child(&fixture.name, rustix::fs::Mode::from_bits_truncate(0o666))
        .unwrap();
    assert!(fixture.manager.admission(&fixture.name).is_err());
    admission
        .chmod_child(&fixture.name, crate::fs::STATE_FILE_MODE)
        .unwrap();
    let record = admission.open_regular_read(&fixture.name).unwrap();
    rustix::fs::fchown(&record, Some(rustix::process::Uid::from_raw(65534)), None).unwrap();
    assert!(fixture.manager.admission(&fixture.name).is_err());
    rustix::fs::fchown(&record, Some(rustix::process::Uid::from_raw(0)), None).unwrap();
    let foreign = format!("{}-foreign", fixture.name);
    admission
        .create_durable_exclusive(
            &foreign,
            Admission::Allowed.bytes(),
            crate::fs::STATE_FILE_MODE,
        )
        .unwrap();
    assert!(fixture.manager.observe_registration(&foreign).is_err());
    assert_eq!(
        admission.read_regular(&foreign).unwrap(),
        Admission::Allowed.bytes()
    );
    admission.remove_file(&foreign).unwrap();
    configuration.remove_file(&overrides[1]).unwrap();
    fixture.assert_observed(ServiceStart::Disabled, ServiceRuntimeState::Stopped);
    fixture.dispose();
}

#[rstest::rstest]
#[case::direct(false)]
#[case::supervised(true)]
#[ignore = "requires a disposable stock OpenRC cgroup-v2 VM as root"]
fn stock_admission_drains_and_blocks_concurrent_starts(#[case] supervised: bool) {
    let mut fixture = AdmissionFixture::new(supervised, ServiceStart::Automatic);
    let start = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &fixture.state,
        false,
        true,
    );
    zup_transaction::apply_service_control(&mut fixture.manager, &start).unwrap();
    let disabled = fixture.state(ServiceStart::Disabled);
    let stop = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &disabled,
        true,
        false,
    );
    let mut requests = Vec::new();
    for _ in 0..4 {
        requests.push(std::process::Command::new("/bin/sh").args(["-c", "for i in 1 2 3 4 5 6 7 8; do /sbin/rc-service --nodeps \"$1\" start >/dev/null 2>&1; done; exit 0", "native-starts", &fixture.name]).spawn().unwrap());
    }
    zup_transaction::apply_service_control(&mut fixture.manager, &stop).unwrap();
    for mut request in requests {
        assert!(request.wait().unwrap().success());
    }
    fixture.assert_observed(ServiceStart::Disabled, ServiceRuntimeState::Stopped);
    fixture.assert_blocked();
    let directory = OwnedDirectory::open(Path::new("/run/openrc/exclusive")).unwrap();
    directory
        .create_durable_exclusive(
            &fixture.name,
            b"",
            rustix::fs::Mode::from_bits_truncate(0o644),
        )
        .unwrap();
    let lease = directory.open_regular_read(&fixture.name).unwrap();
    rustix::fs::flock(&lease, rustix::fs::FlockOperation::LockExclusive).unwrap();
    assert!(
        fixture
            .manager
            .drain_admission(&stop, std::time::Instant::now() + Duration::from_millis(50))
            .is_err()
    );
    drop(lease);
    fixture
        .manager
        .drain_admission(&stop, std::time::Instant::now() + Duration::from_secs(1))
        .unwrap();
    directory.remove_file(&fixture.name).unwrap();
    fixture.dispose();
}

#[test]
#[ignore = "requires the stock OpenRC VM and its native admission delay fixture"]
fn stock_admission_waits_for_an_inflight_native_start() {
    let mut fixture = AdmissionFixture::new(false, ServiceStart::Manual);
    let disabled = fixture.state(ServiceStart::Disabled);
    let stop = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &disabled,
        false,
        false,
    );
    let ready = fixture.root.join("native-admission-ready");
    let mut request = std::process::Command::new("/sbin/rc-service")
        .args(["--nodeps", &fixture.name, "start"])
        .env("LD_PRELOAD", "/root/zup-openrc-admission.so")
        .env(
            "ZUP_PROBE_LOCK",
            format!("/run/openrc/exclusive/{}", fixture.name),
        )
        .env("ZUP_PROBE_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(request.id() as i32).unwrap();
    let process = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            request.try_wait().unwrap().is_none(),
            "native start exited before acquiring admission"
        );
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", request.id())).unwrap();
        if ready.exists()
            && stat
                .rsplit_once(") ")
                .unwrap()
                .1
                .split_ascii_whitespace()
                .next()
                == Some("T")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native start did not pause under its admission lock"
        );
        std::thread::park_timeout(Duration::from_millis(10));
    }
    assert!(zup_transaction::apply_service_control(&mut fixture.manager, &stop).is_err());
    assert_eq!(
        fixture.manager.admission(&fixture.name).unwrap(),
        Some(Admission::Inhibited)
    );
    assert!(request.try_wait().unwrap().is_none());
    rustix::process::pidfd_send_signal(&process, rustix::process::Signal::CONT).unwrap();
    assert!(!request.wait().unwrap().success());
    assert!(!ProcessScope::observe(&fixture.name).unwrap().populated);
    assert!(matches!(
        zup_transaction::reconcile_service_control(&mut fixture.manager, &stop, None).unwrap(),
        zup_transaction::ReconcileResult::AppliedWithReceipt(_)
    ));
    fixture.assert_observed(ServiceStart::Disabled, ServiceRuntimeState::Stopped);
    fixture.assert_blocked();
    fixture.dispose();
}

#[test]
#[ignore = "requires a disposable stock OpenRC cgroup-v2 VM as root"]
fn stock_admission_preserve_recovery_does_not_close_admission() {
    let mut fixture = AdmissionFixture::new(true, ServiceStart::Automatic);
    let start = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &fixture.state,
        false,
        true,
    );
    zup_transaction::apply_service_control(&mut fixture.manager, &start).unwrap();
    let mut preserve = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &fixture.state,
        true,
        true,
    );
    preserve.intent = zup_plan::ServiceControlIntent::Preserve;
    let zup_transaction::ReconcileResult::AppliedWithReceipt(
        zup_transaction::OperationReceipt::ServiceControl(receipt),
    ) = zup_transaction::reconcile_service_control(&mut fixture.manager, &preserve, None).unwrap()
    else {
        panic!("runtime preservation must recover its original execution")
    };
    assert_eq!(
        receipt.execution_generation,
        preserve.before_execution_generation
    );
    assert_eq!(
        fixture.manager.admission(&fixture.name).unwrap(),
        Some(Admission::Allowed)
    );
    fixture.assert_observed(ServiceStart::Automatic, ServiceRuntimeState::Running);
    fixture.dispose();
}

#[rstest::rstest]
#[case::admission("native_admission_inhibited", false)]
#[case::activation("native_configuration_changed", false)]
#[case::stop("native_request_submitted", false)]
#[case::restore("native_configuration_changed", true)]
#[ignore = "requires a disposable stock OpenRC cgroup-v2 VM as root"]
fn stock_admission_recovers_interrupted_transitions(#[case] point: &str, #[case] restore: bool) {
    const CHILD: &str = "ZUP_ADMISSION_OPERATION";
    if let Some(path) = std::env::var_os(CHILD) {
        let operation = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut manager = Openrc::connect(crate::locations::MachineRoots::production()).unwrap();
        zup_transaction::apply_service_control(&mut manager, &operation).unwrap();
        panic!("fault injection must terminate the native request");
    }
    let mut fixture = AdmissionFixture::new(true, ServiceStart::Automatic);
    let start = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &fixture.state,
        false,
        true,
    );
    zup_transaction::apply_service_control(&mut fixture.manager, &start).unwrap();
    let disabled = fixture.state(ServiceStart::Disabled);
    let stop = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &fixture.state,
        &disabled,
        true,
        false,
    );
    let operation = if restore {
        zup_transaction::apply_service_control(&mut fixture.manager, &stop).unwrap();
        control(
            &fixture.manager,
            &fixture.key,
            &fixture.name,
            &disabled,
            &fixture.state,
            false,
            true,
        )
    } else {
        stop
    };
    let path = fixture.root.join("operation.json");
    std::fs::write(&path, serde_json::to_vec(&operation).unwrap()).unwrap();
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "ulimit -c 0; exec \"$@\"", "admission-transition"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            std::thread::current().name().unwrap(),
            "--nocapture",
        ])
        .env(CHILD, &path)
        .env("ZUP_TEST_FAULT_POINT", point)
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .env("ZUP_TEST_FAULT_OPERATION", &fixture.name)
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        child.status.signal(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let mut recovered = Openrc::connect(crate::locations::MachineRoots::production()).unwrap();
    let zup_transaction::ReconcileResult::AppliedWithReceipt(
        zup_transaction::OperationReceipt::ServiceControl(receipt),
    ) = zup_transaction::reconcile_service_control(&mut recovered, &operation, None).unwrap()
    else {
        panic!("native request must have an authoritative recovery receipt")
    };
    assert_eq!(
        receipt.completion,
        if restore {
            zup_transaction::ServiceControlCompletion::CompensationOnly
        } else {
            zup_transaction::ServiceControlCompletion::Reached
        }
    );
    if restore {
        for _ in 0..2 {
            zup_transaction::rollback_service_control(&mut recovered, &receipt).unwrap();
            assert!(matches!(
                zup_transaction::reconcile_service_rollback(&mut recovered, &receipt).unwrap(),
                zup_transaction::RollbackReconcileResult::Restored
            ));
        }
    } else {
        assert!(matches!(
            zup_transaction::reconcile_service_control(&mut recovered, &operation, Some(&receipt))
                .unwrap(),
            zup_transaction::ReconcileResult::Applied
        ));
    }
    fixture.assert_observed(ServiceStart::Disabled, ServiceRuntimeState::Stopped);
    assert_eq!(
        fixture.manager.runlevel_policy(&fixture.name).unwrap(),
        RunlevelPolicy::Manual
    );
    fixture.assert_blocked();
    let restore = control(
        &fixture.manager,
        &fixture.key,
        &fixture.name,
        &disabled,
        &fixture.state,
        false,
        true,
    );
    zup_transaction::apply_service_control(&mut recovered, &restore).unwrap();
    fixture.assert_observed(ServiceStart::Automatic, ServiceRuntimeState::Running);
    fixture.dispose();
}

#[test]
#[ignore = "requires the prepare and verify phases across a disposable OpenRC VM reboot"]
fn boot_admission() {
    let path = Path::new("/root/zup-admission-boot.json");
    match std::env::var("ZUP_ADMISSION_BOOT_PHASE").unwrap().as_str() {
        "prepare" => {
            let automatic = AdmissionFixture::new(true, ServiceStart::Automatic);
            let disabled = AdmissionFixture::new(false, ServiceStart::Automatic);
            disabled
                .manager
                .set_admission(&disabled.name, Admission::Inhibited)
                .unwrap();
            // A crash before runlevel removal must remain inhibited at the next boot.
            assert_eq!(
                disabled.manager.runlevel_policy(&disabled.name).unwrap(),
                RunlevelPolicy::Automatic
            );
            disabled.assert_blocked();
            let fixtures = [automatic, disabled]
                .map(|fixture| (fixture.key, fixture.name, fixture.root, fixture.state));
            std::fs::write(path, serde_json::to_vec(&fixtures).unwrap()).unwrap();
        }
        "verify" => {
            let fixtures: [(ResourceKey, String, PathBuf, ServiceState); 2] =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let [automatic, disabled] = fixtures.map(|(key, name, root, state)| AdmissionFixture {
                manager: Openrc::connect(crate::locations::MachineRoots::production()).unwrap(),
                key,
                name,
                root,
                state,
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            while !matches!(
                automatic.manager.observe_registration(&automatic.name),
                Ok(ObservedServiceState::Service {
                    runtime_state: Some(ServiceRuntimeState::Running),
                    ..
                })
            ) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "automatic native startup did not settle after reboot"
                );
                std::thread::park_timeout(Duration::from_millis(100));
            }
            automatic.assert_observed(ServiceStart::Automatic, ServiceRuntimeState::Running);
            disabled.assert_observed(ServiceStart::Disabled, ServiceRuntimeState::Stopped);
            disabled.assert_blocked();
            disabled
                .manager
                .set_startup(&disabled.name, &disabled.state(ServiceStart::Disabled))
                .unwrap();
            assert_eq!(
                disabled.manager.runlevel_policy(&disabled.name).unwrap(),
                RunlevelPolicy::Manual
            );
            automatic.dispose();
            disabled.dispose();
            std::fs::remove_file(path).unwrap();
        }
        phase => panic!("unrecognized boot phase {phase}"),
    }
}
