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
#[case::windows("windows-x64", false)]
#[case::linux("linux-x64", true)]
fn sdk_service_policies_survive_component_linking_and_execution(
    #[case] target: &str,
    #[case] notify: bool,
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
            selected_components: vec!["service".into()],
        })
        .unwrap()
        .unwrap();
    let service = plan
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceItem::Service(service) => Some(service),
            _ => None,
        })
        .unwrap();
    assert_eq!(service.lifecycle.install, InitialServiceState::Running);
    assert_eq!(service.lifecycle.change, ChangedServiceState::Preserve);
    assert_eq!(service.lifecycle.start_timeout_seconds, 17);
    assert_eq!(service.lifecycle.stop_timeout_seconds, 23);
    assert_eq!(service.requires_files, ["${install}/configure.txt"]);
    assert_eq!(
        service.readiness,
        if notify {
            ServiceReadiness::Notify
        } else {
            ServiceReadiness::Manager
        }
    );
}
