#![cfg(feature = "compiler")]

use rstest::rstest;

use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};
use zup_plugin_contract::{ContractError, PluginEngine, engine_fingerprint};

const VALID_WIT: &str = zup_plugin_abi::WIT_PACKAGE;

fn component_for_wit(wit: &str) -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8, false).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn component_wat(wat: &str) -> Vec<u8> {
    wat::parse_str(wat).unwrap()
}

#[rstest]
#[case::missing_export("(component)", "missing_export")]
#[case::extra_export(
    r#"(component (type $t (func)) (export "extra" (type $t)))"#,
    "extra_export"
)]
#[case::any_import(r#"(component (import "env" (func)))"#, "any_import")]
#[case::core_module("(module)", "core_module")]
fn a_component_of_the_wrong_shape_is_refused(#[case] wat: &str, #[case] refusal: &str) {
    let engine = PluginEngine::host().unwrap();
    let error = engine.compile_component(&component_wat(wat)).unwrap_err();
    assert_eq!(refusal_of(&error), refusal, "{error:?}");
}

#[rstest]
#[case::extra_planner_function(
    VALID_WIT.replace(
        "  plan: func(context: context) -> result<installation-plan, plugin-error>;",
        "  extra: func();\n\n  plan: func(context: context) -> result<installation-plan, plugin-error>;",
    ),
    "unexpected_plan_export"
)]
#[case::wrong_signature(
    r#"
package zup:plugin@1.0.0;

interface planner {
    plan: func();
}

world plugin {
    export planner;
}
"#,
    "signature"
)]
fn a_world_that_does_not_match_the_contract_is_refused(#[case] wit: String, #[case] refusal: &str) {
    assert_ne!(wit, VALID_WIT, "the fixture must actually change the world");
    let engine = PluginEngine::host().unwrap();
    let error = engine
        .compile_component(&component_for_wit(&wit))
        .unwrap_err();
    assert_eq!(refusal_of(&error), refusal, "{error:?}");
}

fn refusal_of(error: &ContractError) -> &'static str {
    match error {
        ContractError::CoreModule => "core_module",
        ContractError::UnexpectedImport { name } => {
            assert_eq!(name, "env");
            "any_import"
        }
        ContractError::MissingExport { expected } => {
            assert_eq!(*expected, "zup:plugin/planner@1.0.0");
            "missing_export"
        }
        ContractError::UnexpectedExport { name } | ContractError::UnexpectedPlanExport { name } => {
            assert_eq!(name, "extra");
            if matches!(error, ContractError::UnexpectedExport { .. }) {
                "extra_export"
            } else {
                "unexpected_plan_export"
            }
        }
        ContractError::Signature { .. } => "signature",
        other => panic!("not a shape refusal: {other:?}"),
    }
}

#[rstest]
#[case::arbitrary_bytes(&b"not aot"[..])]
#[case::raw_core_module(&component_wat("(module)"))]
#[case::component_that_was_never_precompiled(&component_for_wit(VALID_WIT))]
fn a_precompiled_output_the_engine_did_not_produce_is_refused(#[case] aot: &[u8]) {
    let engine = PluginEngine::host().unwrap();
    let error = engine.verify_precompiled(aot).unwrap_err();
    assert!(
        matches!(error, ContractError::TrustedAot { .. }),
        "{error:?}"
    );
}

#[test]
fn the_fingerprint_covers_the_contract_and_the_engine_configuration() {
    let first = engine_fingerprint("x86_64-pc-windows-msvc");
    assert_eq!(
        first,
        engine_fingerprint("x86_64-pc-windows-msvc"),
        "one configuration is one fingerprint"
    );
    assert_ne!(
        first,
        engine_fingerprint("aarch64-pc-windows-msvc"),
        "the target is part of what an engine is"
    );
    assert_eq!(
        first.to_hex(),
        "bb10e7c2044e5f55ace87ae79d15a4f4846e61d3614101e3dbff11b0f0a918ef"
    );
}
