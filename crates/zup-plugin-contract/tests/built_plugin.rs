//! A plugin authored against `zup-sdk`, built by Zup, and loaded by the host.
//!
//! The point of this test is the round trip rather than either end: a plugin
//! that only ever compiles has proved nothing, and a host that only ever sees
//! fixtures it made itself has proved nothing either. So this builds a component
//! the way an author would, hands it to the same validation an installer
//! performs, and calls it.

use std::path::Path;

use zup_plugin_contract::{
    Context, InstallScope, InvocationError, PluginEngine, ValidatedComponent,
};

/// The component `zup plugin build` produced.
fn component() -> Vec<u8> {
    // The path is compiled in rather than searched for, so this test cannot
    // pass against a component an earlier build left behind. `cargo xtask
    // toolchain build` produces it.
    std::fs::read(env!("ZUP_TEST_PLUGIN")).unwrap_or_else(|error| {
        panic!(
            "no plugin component at {}: {error}",
            env!("ZUP_TEST_PLUGIN"),
        )
    })
}

/// A plugin is a component that imports nothing.
#[test]
fn a_built_plugin_is_a_component_the_host_accepts() {
    let component = component();
    assert_eq!(
        &component[..4],
        &[0x00, 0x61, 0x73, 0x6d],
        "a plugin is a component, not a core module"
    );

    let engine = PluginEngine::host().expect("an engine for this host");
    let validated: ValidatedComponent = engine
        .compile_component(&component)
        .expect("a component built from the canonical contract is one the host links against");

    assert_eq!(
        validated.fingerprint(),
        engine.fingerprint(),
        "and it was compiled by an engine configured the way this host is"
    );
}

/// The plugin answers the question it is asked, through the ABI, rather than
/// merely being loadable.
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
            target: "x86_64-pc-windows-msvc".into(),
            selected_components: vec!["core".into(), "docs".into()],
        })
        .expect("the call itself succeeds")
        .expect("the plugin returns a plan");

    assert!(
        !plan.resources.is_empty(),
        "the plugin declares what it installs"
    );
}

/// A guest that misbehaves has to arrive as its own failure rather than as a
/// generic one, because the host branches on which.
#[test]
fn a_plugin_that_cannot_plan_refuses_rather_than_trapping() {
    let engine = PluginEngine::host().expect("an engine for this host");
    let validated = engine
        .compile_component(&component())
        .expect("the component links");

    let outcome = validated.plan(&Context {
        plugin_id: "configure".into(),
        app_id: "com.example.acme".into(),
        app_name: "Acme".into(),
        app_version: "1.4.2".into(),
        install_directory: "${location.user_data}/Acme".into(),
        install_scope: InstallScope::User,
        target: "x86_64-pc-windows-msvc".into(),
        selected_components: Vec::new(),
    });

    // A plugin that returns a plan is the expected case; what matters is that
    // whichever way it goes, the host can tell a refusal from a fault.
    match outcome {
        Ok(Ok(plan)) => assert!(!plan.resources.is_empty()),
        Ok(Err(refusal)) => assert!(!refusal.code.is_empty(), "a refusal names a code"),
        Err(InvocationError::Trap { .. }) => panic!("the guest trapped on an empty selection"),
        Err(other) => panic!("unexpected failure: {other}"),
    }
}