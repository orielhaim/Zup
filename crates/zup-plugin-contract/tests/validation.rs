#![cfg(feature = "compiler")]

use std::sync::atomic::{AtomicUsize, Ordering};

use rstest::rstest;

use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};
use zup_plugin_contract::{
    Context, ContractError, InstallScope, InvocationError, PluginEngine, engine_fingerprint,
};

const VALID_WIT: &str = zup_plugin_abi::WIT_PACKAGE;

fn component_for_wit(wit: &str) -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    // `false` matches the encoder below, which leaves canonical names off.
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

fn component_for_body(body: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
            (type (func (param i32) (result i32)))
            (type (func (param i32)))
            (type (func (param i32 i32 i32 i32) (result i32)))
            (type (func))
            (memory (export "cm32p2_memory") 1)
            (func (export "cm32p2|zup:plugin/planner@1|plan") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
                {body}
            )
            (func (export "cm32p2|zup:plugin/planner@1|plan_post") (param i32))
            (func (export "cm32p2_realloc") (param i32 i32 i32 i32) (result i32)
                local.get 0
            )
            (func (export "cm32p2_initialize"))
            (data (i32.const 2048) "bad")
            (data (i32.const 2064) "failure")
            (data (i32.const 3000) "file")
            (data (i32.const 3064) "failure")
            (data (i32.const 4000) "\01\02\03")
        )"#
    );
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", VALID_WIT).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = component_wat(&wat);
    // `false` matches the encoder below, which leaves canonical names off.
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8, false).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn context() -> Context {
    Context {
        plugin_id: "helper".to_owned(),
        app_id: "com.example.app".to_owned(),
        app_name: "App".to_owned(),
        app_version: "1.0.0".to_owned(),
        install_directory: "${location.user_data}/App".to_owned(),
        install_scope: InstallScope::User,
        target: "x86_64-pc-windows-msvc".to_owned(),
        selected_components: Vec::new(),
    }
}

/// Every way a guest can fail has to arrive as its own `InvocationError`. These are the
/// ABI's error-mapping rules, and they are what a host branches on: a caller that cannot
/// tell a fuel exhaustion from a trap cannot decide whether to retry, and one that sees
/// a generic "the plugin failed" cannot tell a plugin's own refusal from a host fault.
#[rstest]
#[case::oversized_output(
    "(drop (memory.grow (i32.const 129))) (i32.store (i32.const 1024) (i32.const 1)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 8388609)) (i32.store (i32.const 1036) (i32.const 0)) (i32.store (i32.const 1040) (i32.const 0)) (i32.const 1024)",
    Refusal::OutputLimit,
    Never::Ask
)]
#[case::trap("unreachable", Refusal::Trap, Never::Ask)]
#[case::out_of_fuel("(loop (br 0)) unreachable", Refusal::FuelExhausted, Never::Ask)]
#[case::out_of_memory(
    "(drop (memory.grow (i32.const 513))) (i32.const 0)",
    Refusal::MemoryLimit,
    Never::Ask
)]
#[case::cancelled("(loop (br 0)) unreachable", Refusal::Cancelled, Never::AfterFirstPoll)]
fn a_guest_that_misbehaves_is_mapped_to_its_own_failure(
    #[case] body: &str,
    #[case] expected: Refusal,
    #[case] cancellation: Never,
) {
    let engine = PluginEngine::host().unwrap();
    let component = engine.compile_component(&component_for_body(body)).unwrap();
    let calls = AtomicUsize::new(0);
    let error = match cancellation {
        Never::Ask => component.plan(&context()).unwrap_err(),
        Never::AfterFirstPoll => component
            .plan_with_cancellation(&context(), &|| calls.fetch_add(1, Ordering::Relaxed) > 0)
            .unwrap_err(),
    };
    assert!(
        expected.matches(&error),
        "a guest running `{body}` should fail as {expected:?}, got {error:?}"
    );
}

/// The failure an invocation is expected to produce. Named rather than constructed
/// because `Trap` carries a message the fixture does not have to predict, and what the
/// mapping rules promise is *which* failure, not what it says.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    OutputLimit,
    Trap,
    FuelExhausted,
    MemoryLimit,
    Cancelled,
}

impl Refusal {
    fn matches(self, error: &InvocationError) -> bool {
        matches!(
            (self, error),
            (Refusal::OutputLimit, InvocationError::OutputLimit { .. })
                | (Refusal::Trap, InvocationError::Trap { .. })
                | (Refusal::FuelExhausted, InvocationError::FuelExhausted)
                | (Refusal::MemoryLimit, InvocationError::MemoryLimit)
                | (Refusal::Cancelled, InvocationError::Cancelled)
        )
    }
}

/// Whether the invocation is given a cancellation query at all. The fuel fixture spins
/// forever, so the two cases differ only in whether anything is watching.
#[derive(Debug, Clone, Copy)]
enum Never {
    Ask,
    AfterFirstPoll,
}

#[test]
fn invocation_returns_all_resource_families() {
    let engine = PluginEngine::host().unwrap();
    let body = "(i32.store (i32.const 1024) (i32.const 0)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 6)) \
        (i32.store (i32.const 2048) (i32.const 0)) (i32.store (i32.const 2052) (i32.const 3000)) (i32.store (i32.const 2056) (i32.const 4)) (i32.store (i32.const 2060) (i32.const 4000)) (i32.store (i32.const 2064) (i32.const 3)) \
        (i32.store (i32.const 2100) (i32.const 1)) (i32.store (i32.const 2104) (i32.const 0)) (i32.store (i32.const 2108) (i32.const 3000)) (i32.store (i32.const 2112) (i32.const 4)) (i32.store (i32.const 2116) (i32.const 3064)) (i32.store (i32.const 2120) (i32.const 7)) (i32.store (i32.const 2124) (i32.const 0)) (i32.store (i32.const 2128) (i32.const 0)) (i32.store (i32.const 2132) (i32.const 0)) (i32.store (i32.const 2136) (i32.const 0)) \
        (i32.store (i32.const 2152) (i32.const 2)) (i32.store (i32.const 2156) (i32.const 3000)) (i32.store (i32.const 2160) (i32.const 4)) \
        (i32.store (i32.const 2204) (i32.const 3)) (i32.store (i32.const 2208) (i32.const 3000)) (i32.store (i32.const 2212) (i32.const 4)) (i32.store (i32.const 2216) (i32.const 3064)) (i32.store (i32.const 2220) (i32.const 7)) (i32.store (i32.const 2224) (i32.const 0)) (i32.store (i32.const 2228) (i32.const 0)) (i32.store (i32.const 2232) (i32.const 0)) (i32.store (i32.const 2236) (i32.const 3000)) (i32.store (i32.const 2240) (i32.const 4)) (i32.store (i32.const 2244) (i32.const 0)) (i32.store (i32.const 2248) (i32.const 0)) (i32.store (i32.const 2252) (i32.const 1)) \
        (i32.store (i32.const 2256) (i32.const 4)) (i32.store (i32.const 2260) (i32.const 3000)) (i32.store (i32.const 2264) (i32.const 4)) (i32.store (i32.const 2268) (i32.const 3064)) (i32.store (i32.const 2272) (i32.const 7)) (i32.store (i32.const 2276) (i32.const 0)) (i32.store (i32.const 2280) (i32.const 0)) \
        (i32.store (i32.const 2308) (i32.const 5)) (i32.store (i32.const 2312) (i32.const 3000)) (i32.store (i32.const 2316) (i32.const 4)) (i32.store (i32.const 2320) (i32.const 3064)) (i32.store (i32.const 2324) (i32.const 7)) (i32.store (i32.const 2328) (i32.const 0)) (i32.store (i32.const 2332) (i32.const 0)) (i32.store (i32.const 2336) (i32.const 0)) (i32.store (i32.const 2340) (i32.const 3000)) (i32.store (i32.const 2344) (i32.const 4)) (i32.const 1024)";
    let component = engine.compile_component(&component_for_body(body)).unwrap();
    let plan = component.plan(&context()).unwrap().unwrap();
    assert_eq!(plan.resources.len(), 6);
}

#[test]
fn invocation_preserves_guest_rejection() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body(
            "(i32.store (i32.const 1024) (i32.const 1)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 3)) (i32.store (i32.const 1036) (i32.const 2064)) (i32.store (i32.const 1040) (i32.const 7)) (i32.const 1024)",
        ))
        .unwrap();
    let error = component.plan(&context()).unwrap().unwrap_err();
    assert_eq!(error.code, "bad");
    assert_eq!(error.message, "failure");
}

/// A component is accepted or refused on its shape, and each shape below is one a
/// hand-written or generated component can actually arrive in. Nothing here runs the
/// guest: these are all decided before a plugin ever executes.
/// A component is refused on its shape, and each shape below is one a hand-written or
/// generated component can actually arrive in. Nothing here runs the guest: every one of
/// these is decided before a plugin ever executes.
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

/// A world that names a second planner function, and a world whose `plan` has the wrong
/// signature: the first because the export set is closed, the second because the
/// canonical ABI is what the host links against.
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

/// Which refusal the contract layer produced, and whether it named the offending symbol.
/// Naming the variant is what lets the two tables above read as tables; the payload is
/// checked here because a refusal that does not say which import or export was refused
/// leaves the plugin author nothing to act on.
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

/// Only a component this engine's own cranelift precompiled may be loaded. A raw module
/// is not a component, and a component is not trusted merely for being one: the host
/// links against the exact artifact shape its engine produces, so anything else is
/// refused before a deserializer is handed a byte of it.
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

/// The fingerprint is what stops a host from loading output a differently
/// configured engine produced, so two calls have to agree and a change to any
/// input has to move it.
///
/// The expected value is pinned rather than compared to a second call: the
/// point is that the digest covers its inputs, and the WIT contract is one of
/// them. A change here means the contract or the engine configuration changed,
/// which is exactly the event an ahead-of-time artifact has to be rebuilt for.
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
        "55c7a6d0e0e7bd14f5cdd0b3a6176a2691a7ae54b99efa994ae4954990318a08"
    );
}
