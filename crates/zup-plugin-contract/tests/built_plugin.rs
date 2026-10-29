use zup_plugin_contract::{Context, InstallScope, PluginEngine};

fn component() -> Vec<u8> {
    std::fs::read(env!("ZUP_TEST_PLUGIN")).unwrap_or_else(|error| {
        panic!(
            "no plugin component at {}: {error}",
            env!("ZUP_TEST_PLUGIN"),
        )
    })
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
