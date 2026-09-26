#![cfg(feature = "compiler")]

use std::sync::atomic::{AtomicUsize, Ordering};

use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};
use zup_plugin_contract::{
    Context, ContractError, EngineFingerprint, HOST_TARGET, InstallScope, InvocationError,
    PluginEngine, engine_fingerprint,
};

const VALID_WIT: &str = include_str!("../../../wit/zup-plugin.wit");

fn component_for_wit(wit: &str) -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
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
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
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

#[test]
fn accepts_valid_contract_shape() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_wit(VALID_WIT))
        .unwrap();
    assert_eq!(component.fingerprint(), engine.fingerprint());
}

#[test]
fn invocation_returns_an_empty_plan() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body(
            "(i32.store (i32.const 1024) (i32.const 0)) (i32.store (i32.const 1028) (i32.const 0)) (i32.store (i32.const 1032) (i32.const 0)) (i32.const 1024)",
        ))
        .unwrap();
    let result = component.plan(&context()).unwrap();
    assert!(result.unwrap().resources.is_empty());
}

#[test]
fn invocation_maps_oversized_guest_error_to_output_limit() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body(
            "(drop (memory.grow (i32.const 129))) (i32.store (i32.const 1024) (i32.const 1)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 8388609)) (i32.store (i32.const 1036) (i32.const 0)) (i32.store (i32.const 1040) (i32.const 0)) (i32.const 1024)",
        ))
        .unwrap();
    let error = component.plan(&context()).unwrap_err();
    assert!(matches!(error, InvocationError::OutputLimit { .. }));
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
fn invocation_returns_one_resource() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body(
            "(i32.store (i32.const 1024) (i32.const 0)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 1)) (i32.store (i32.const 2048) (i32.const 0)) (i32.store (i32.const 2052) (i32.const 3000)) (i32.store (i32.const 2056) (i32.const 6)) (i32.store (i32.const 2060) (i32.const 4000)) (i32.store (i32.const 2064) (i32.const 3)) (i32.const 1024)",
        ))
        .unwrap();
    let plan = component.plan(&context()).unwrap().unwrap();
    assert_eq!(plan.resources.len(), 1);
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

#[test]
fn invocation_maps_trap_without_text_matching() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body("unreachable"))
        .unwrap();
    let error = component.plan(&context()).unwrap_err();
    assert!(matches!(error, InvocationError::Trap { .. }));
}

#[test]
fn invocation_maps_cancellation() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body("(loop (br 0)) unreachable"))
        .unwrap();
    let calls = AtomicUsize::new(0);
    let cancellation = || calls.fetch_add(1, Ordering::Relaxed) > 0;
    let error = component
        .plan_with_cancellation(&context(), &cancellation)
        .unwrap_err();
    assert_eq!(error, InvocationError::Cancelled);
}

#[test]
fn invocation_maps_fuel_exhaustion() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body("(loop (br 0)) unreachable"))
        .unwrap();
    let error = component.plan(&context()).unwrap_err();
    assert_eq!(error, InvocationError::FuelExhausted);
}

#[test]
fn invocation_maps_memory_growth_rejection() {
    let engine = PluginEngine::host().unwrap();
    let component = engine
        .compile_component(&component_for_body(
            "(drop (memory.grow (i32.const 513))) (i32.const 0)",
        ))
        .unwrap();
    let error = component.plan(&context()).unwrap_err();
    assert_eq!(error, InvocationError::MemoryLimit);
}

#[test]
fn rejects_missing_export() {
    let engine = PluginEngine::host().unwrap();
    let component = component_wat("(component)");
    let error = engine.compile_component(&component).unwrap_err();
    assert_eq!(
        error,
        ContractError::MissingExport {
            expected: "zup:plugin/planner@1.0.0",
        }
    );
}

#[test]
fn rejects_extra_export() {
    let engine = PluginEngine::host().unwrap();
    let component = component_wat(r#"(component (type $t (func)) (export "extra" (type $t)))"#);
    let error = engine.compile_component(&component).unwrap_err();
    assert_eq!(
        error,
        ContractError::UnexpectedExport {
            name: "extra".to_owned(),
        }
    );
}

#[test]
fn rejects_extra_planner_function() {
    let engine = PluginEngine::host().unwrap();
    let wit = VALID_WIT.replace(
        "  plan: func(context: context) -> result<installation-plan, plugin-error>;",
        "  extra: func();\n\n  plan: func(context: context) -> result<installation-plan, plugin-error>;",
    );
    assert_ne!(wit, VALID_WIT);
    let error = engine
        .compile_component(&component_for_wit(&wit))
        .unwrap_err();
    assert_eq!(
        error,
        ContractError::UnexpectedPlanExport {
            name: "extra".to_owned(),
        }
    );
}

#[test]
fn rejects_any_import() {
    let engine = PluginEngine::host().unwrap();
    let component = component_wat(r#"(component (import "env" (func)))"#);
    let error = engine.compile_component(&component).unwrap_err();
    assert_eq!(
        error,
        ContractError::UnexpectedImport {
            name: "env".to_owned(),
        }
    );
}

#[test]
fn rejects_core_module() {
    let engine = PluginEngine::host().unwrap();
    let module = component_wat("(module)");
    let error = engine.compile_component(&module).unwrap_err();
    assert_eq!(error, ContractError::CoreModule);
}

#[test]
fn rejects_wrong_wit_signature() {
    let engine = PluginEngine::host().unwrap();
    let component = component_for_wit(
        r#"
        package zup:plugin@1.0.0;

        interface planner {
            plan: func();
        }

        world plugin {
            export planner;
        }
        "#,
    );
    let error = engine.compile_component(&component).unwrap_err();
    assert!(matches!(error, ContractError::Signature { .. }));
}

#[test]
fn round_trips_precompiled_aot_with_the_same_fingerprint() {
    let compiler = PluginEngine::new(HOST_TARGET).unwrap();
    let aot = compiler
        .precompile_component(&component_for_wit(VALID_WIT))
        .unwrap();

    let runtime = PluginEngine::new(HOST_TARGET).unwrap();
    assert!(wasmtime::Engine::detect_precompiled(&aot).is_some());
    let component = runtime
        .compile_component(&component_for_wit(VALID_WIT))
        .unwrap();
    assert_eq!(component.fingerprint(), runtime.fingerprint());
}

#[test]
fn rejects_wrong_precompiled_output() {
    let engine = PluginEngine::host().unwrap();
    let error = engine.verify_precompiled(b"not aot").unwrap_err();
    assert!(matches!(error, ContractError::TrustedAot { .. }));
}

#[test]
fn rejects_raw_webassembly_before_deserialize() {
    let engine = PluginEngine::host().unwrap();
    let module = component_wat("(module)");
    let error = engine.verify_precompiled(&module).unwrap_err();
    assert!(matches!(error, ContractError::TrustedAot { .. }));
}

#[test]
fn fingerprint_is_stable() {
    let first: EngineFingerprint = engine_fingerprint("x86_64-pc-windows-msvc");
    let second = engine_fingerprint("x86_64-pc-windows-msvc");
    assert_eq!(first, second);
    assert_eq!(
        first.to_hex(),
        "bb10e7c2044e5f55ace87ae79d15a4f4846e61d3614101e3dbff11b0f0a918ef"
    );
}
