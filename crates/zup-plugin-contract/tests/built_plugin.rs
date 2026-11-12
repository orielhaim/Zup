use zup_plugin_contract::{Context, InstallScope, PluginEngine};

fn component() -> Vec<u8> {
    zup_plugin_abi::test_fixture::component()
}

#[test]
fn a_built_plugin_answers_the_host() {
    let engine = PluginEngine::host().expect("an engine for this host");
    let validated = engine
        .compile_component(&component())
        .expect("the component links");

    let plan = validated
        .plan(&Context {
            plugin_id: "configure".into(),
            app_id: "com.example.acme".into(),
            app_name: "Acme".into(),
            app_version: "1.4.2".into(),
            install_directory: "${location.user_data}/Acme".into(),
            install_scope: InstallScope::User,
            target: "windows-x64".into(),
            selected_components: vec!["core".into(), "docs".into()],
        })
        .expect("the call itself succeeds")
        .expect("the plugin returns a plan");

    assert!(
        !plan.resources.is_empty(),
        "the plugin declares what it installs"
    );
}

#[rstest::rstest]
#[case::windows("windows-x64", false, false)]
#[case::linux("linux-x64", true, false)]
#[case::windows_failure("windows-x64", false, true)]
#[case::linux_failure("linux-x64", true, true)]
fn sdk_service_policies_survive_component_linking_and_execution(
    #[case] target: &str,
    #[case] notify: bool,
    #[case] failure: bool,
) {
    use zup_plugin_contract::{
        ChangedServiceState, InitialServiceState, ResourceItem, ServiceReadiness,
    };
    let engine = PluginEngine::host().unwrap();
    let validated = engine.compile_component(&component()).unwrap();
    let plan = validated
        .plan(&Context {
            plugin_id: "configure".into(),
            app_id: "com.example.worker".into(),
            app_name: "Worker".into(),
            app_version: "1.0.0".into(),
            install_directory: "${location.programs}/Worker".into(),
            install_scope: InstallScope::Machine,
            target: target.into(),
            selected_components: if failure {
                vec!["service".into(), "failure".into(), "dependencies".into()]
            } else {
                vec!["service".into()]
            },
        })
        .unwrap()
        .unwrap();
    let service = plan
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceItem::Service(service) if service.id == "com.example.worker.worker" => {
                Some(service)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(service.lifecycle.install, InitialServiceState::Running);
    assert_eq!(service.lifecycle.change, ChangedServiceState::Preserve);
    match service.lifecycle.timeouts.as_ref().unwrap() {
        zup_plugin_contract::ServiceTimeouts::Systemd(value) => {
            assert!(notify);
            assert_eq!(
                (
                    value.start_timeout_seconds,
                    value.stop_grace_seconds,
                    value.abort_timeout_seconds,
                    value.settle_seconds
                ),
                (17, 23, 5, 5)
            );
        }
        zup_plugin_contract::ServiceTimeouts::Windows(value) => {
            assert!(!notify);
            assert_eq!(
                (
                    value.start_observation_seconds,
                    value.stop_observation_seconds
                ),
                (17, 23)
            );
        }
        zup_plugin_contract::ServiceTimeouts::Openrc(_) => {
            panic!("fixture declares SCM or systemd")
        }
    }
    assert_eq!(service.requires_files, ["${install}/configure.txt"]);
    assert_eq!(
        service.requires_services,
        if failure {
            vec!["com.example.worker.storage"]
        } else {
            Vec::<&str>::new()
        }
    );
    use zup_plugin_contract::{
        ServiceFailurePolicy, SystemdFailurePolicy, SystemdRestartCondition, WindowsFailurePolicy,
    };
    assert!(matches!(
        (failure, notify, &service.failure),
        (false, _, ServiceFailurePolicy::None)
            | (
                true,
                true,
                ServiceFailurePolicy::Systemd(SystemdFailurePolicy {
                    condition: SystemdRestartCondition::OnFailure,
                    delay_seconds: 2,
                    start_limit_burst: 3,
                    start_limit_interval_seconds: 60,
                }),
            )
            | (
                true,
                false,
                ServiceFailurePolicy::Windows(WindowsFailurePolicy {
                    delay_seconds: 2,
                    restart_attempts: 2,
                    reset_seconds: 60,
                }),
            )
    ));
    assert_eq!(
        service.readiness,
        if notify {
            ServiceReadiness::Notify
        } else {
            ServiceReadiness::Manager
        }
    );
}

fn probe_context(name: &str) -> Context {
    Context {
        plugin_id: "sandbox-probe".into(),
        app_id: "com.example.probe".into(),
        app_name: name.into(),
        app_version: "1.0.0".into(),
        install_directory: "${install}".into(),
        install_scope: InstallScope::User,
        target: "windows-x64".into(),
        selected_components: Vec::new(),
    }
}

#[test]
fn native_wasi_streams_and_guest_failures_are_isolated_per_invocation() {
    use zup_plugin_contract::InvocationError;
    let engine = PluginEngine::host().unwrap();
    let aot = engine.precompile_component(&component()).unwrap();
    let validated = unsafe { engine.validate_trusted_aot(&aot) }.unwrap();
    for name in ["clock", "wait", "exit", "panic"] {
        let error = validated.plan(&probe_context(name)).unwrap_err();
        assert!(
            matches!(error, InvocationError::Trap { .. }),
            "{name}: {error:?}"
        );
        let output = validated
            .plan(&probe_context("stdio"))
            .unwrap()
            .unwrap_err();
        assert_eq!(output.code, "isolated");
        assert_eq!(
            output.message,
            "empty environment, closed input, sink output"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_planning_can_run_inside_a_desktop_async_runtime() {
    let engine = PluginEngine::host().unwrap();
    let validated = engine.compile_component(&component()).unwrap();
    assert_eq!(
        validated
            .plan(&probe_context("stdio"))
            .unwrap()
            .unwrap_err()
            .code,
        "isolated"
    );
}
