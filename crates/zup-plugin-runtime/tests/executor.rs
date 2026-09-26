use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use semver::Version;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;
use zup_build::TargetBuildPlan;
use zup_bundle::{BundleWriter, CompiledPluginArtifact, Package, PluginArtifact};
use zup_core::{
    App, AppId, ComponentId, Frontend, Install, InstallDirectory, InstallScope, Installer,
    NonEmptyString, PluginBinding, PluginId, SelectedScope, Sha256Digest, TargetTriple, Template,
};
use zup_plan::{
    NeverCancelled, PluginExecutor, PluginPlanningContext, PluginResource, PluginResourceProposal,
};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, PluginEngine, WASMTIME_VERSION,
    engine_fingerprint, wit_package_digest,
};
use zup_plugin_runtime::{LoadError, WasmtimePluginExecutor};

const PLUGIN_ID: &str = "helper";
const VALID_WIT: &str = include_str!("../../../wit/zup-plugin.wit");

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
            (data (i32.const 5000) "${{install}}/plugin-config.txt")
            (data (i32.const 6000) "app id: com.example.runtime\0ainstall directory: ${{location.user_data}}/Runtime\0aselected components: core\0a")
        )"#
    );
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", VALID_WIT).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = wat::parse_str(&wat).unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn fixture(name: &str) -> Vec<u8> {
    let body = match name {
        "empty" => {
            r#"(i32.store (i32.const 1024) (i32.const 0)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 0)) (i32.const 1024)"#
        }
        "trap" => "unreachable",
        "fuel" => "(loop (br 0)) unreachable",
        "memory" => "(drop (memory.grow (i32.const 513))) (i32.const 0)",
        "rejected" => {
            r#"(i32.store (i32.const 1024) (i32.const 1)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 3)) (i32.store (i32.const 1036) (i32.const 2064)) (i32.store (i32.const 1040) (i32.const 7)) (i32.const 1024)"#
        }
        "output" => {
            r#"(drop (memory.grow (i32.const 129))) (i32.store (i32.const 1024) (i32.const 1)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 8388609)) (i32.store (i32.const 1036) (i32.const 0)) (i32.store (i32.const 1040) (i32.const 0)) (i32.const 1024)"#
        }
        "all" => {
            r#"(i32.store (i32.const 1024) (i32.const 0)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 6)) (i32.store (i32.const 2048) (i32.const 0)) (i32.store (i32.const 2052) (i32.const 3000)) (i32.store (i32.const 2056) (i32.const 4)) (i32.store (i32.const 2060) (i32.const 4000)) (i32.store (i32.const 2064) (i32.const 3)) (i32.store (i32.const 2100) (i32.const 1)) (i32.store (i32.const 2104) (i32.const 0)) (i32.store (i32.const 2108) (i32.const 3000)) (i32.store (i32.const 2112) (i32.const 4)) (i32.store (i32.const 2116) (i32.const 3064)) (i32.store (i32.const 2120) (i32.const 7)) (i32.store (i32.const 2124) (i32.const 0)) (i32.store (i32.const 2128) (i32.const 0)) (i32.store (i32.const 2132) (i32.const 0)) (i32.store (i32.const 2136) (i32.const 0)) (i32.store (i32.const 2152) (i32.const 2)) (i32.store (i32.const 2156) (i32.const 3000)) (i32.store (i32.const 2160) (i32.const 4)) (i32.store (i32.const 2204) (i32.const 3)) (i32.store (i32.const 2208) (i32.const 3000)) (i32.store (i32.const 2212) (i32.const 4)) (i32.store (i32.const 2216) (i32.const 3064)) (i32.store (i32.const 2220) (i32.const 7)) (i32.store (i32.const 2224) (i32.const 0)) (i32.store (i32.const 2228) (i32.const 0)) (i32.store (i32.const 2232) (i32.const 0)) (i32.store (i32.const 2236) (i32.const 3000)) (i32.store (i32.const 2240) (i32.const 4)) (i32.store (i32.const 2244) (i32.const 0)) (i32.store (i32.const 2248) (i32.const 0)) (i32.store (i32.const 2252) (i32.const 1)) (i32.store (i32.const 2256) (i32.const 4)) (i32.store (i32.const 2260) (i32.const 3000)) (i32.store (i32.const 2264) (i32.const 4)) (i32.store (i32.const 2268) (i32.const 3064)) (i32.store (i32.const 2272) (i32.const 7)) (i32.store (i32.const 2276) (i32.const 0)) (i32.store (i32.const 2280) (i32.const 0)) (i32.store (i32.const 2308) (i32.const 5)) (i32.store (i32.const 2312) (i32.const 3000)) (i32.store (i32.const 2316) (i32.const 4)) (i32.store (i32.const 2320) (i32.const 3064)) (i32.store (i32.const 2324) (i32.const 7)) (i32.store (i32.const 2328) (i32.const 0)) (i32.store (i32.const 2332) (i32.const 0)) (i32.store (i32.const 2336) (i32.const 0)) (i32.store (i32.const 2340) (i32.const 3000)) (i32.store (i32.const 2344) (i32.const 4)) (i32.const 1024)"#
        }
        "configure" => {
            r#"(i32.store (i32.const 1024) (i32.const 0)) (i32.store (i32.const 1028) (i32.const 2048)) (i32.store (i32.const 1032) (i32.const 1)) (i32.store (i32.const 2048) (i32.const 0)) (i32.store (i32.const 2052) (i32.const 5000)) (i32.store (i32.const 2056) (i32.const 28)) (i32.store (i32.const 2060) (i32.const 6000)) (i32.store (i32.const 2064) (i32.const 103)) (i32.const 1024)"#
        }
        _ => panic!("unknown fixture"),
    };
    let component = component_for_body(body);
    PluginEngine::new(HOST_TARGET)
        .unwrap()
        .precompile_component(&component)
        .unwrap()
}

fn empty_aot() -> Vec<u8> {
    fixture("empty")
}

fn host_target() -> TargetTriple {
    TargetTriple::parse(HOST_TARGET).unwrap()
}

fn installer() -> Installer {
    Installer {
        ui: None,
        frontend: Frontend::Gui,
        app: App {
            id: AppId::new("com.example.runtime").unwrap(),
            name: NonEmptyString::new("Runtime").unwrap(),
            version: Version::parse("1.0.0").unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        target: host_target(),
        updates: None,
        prerequisites: Vec::new(),
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse("${location.user_data}/Runtime").unwrap()),
                machine: None,
            },
            allow_directory_override: false,
        },
        components: Vec::new(),
        plugins: vec![PluginBinding {
            id: PluginId::new(PLUGIN_ID).unwrap(),
            component: None,
            when: None,
        }],
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    }
}

fn plan(target: TargetTriple) -> TargetBuildPlan {
    let mut installer = installer();
    installer.target = target;
    TargetBuildPlan {
        installer,
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        total_size: 0,
        prerequisite_size: 0,
    }
}

fn artifact(bytes: &[u8], target: &TargetTriple) -> CompiledPluginArtifact {
    let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    CompiledPluginArtifact::new(
        PluginArtifact {
            plugin_id: PluginId::new(PLUGIN_ID).unwrap(),
            source_size: bytes.len() as u64,
            source_sha256: digest,
            target: target.clone(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(
                *engine_fingerprint(target.as_str()).as_bytes(),
            ),
            aot_size: bytes.len() as u64,
            aot_sha256: digest,
            blob: digest,
        },
        bytes.to_vec(),
    )
    .unwrap()
}

fn build_package(bytes: &[u8], target: &TargetTriple) -> Package {
    let package = BundleWriter::encode(&plan(target.clone()), &[artifact(bytes, target)]).unwrap();
    Package::parse(&package).unwrap()
}

fn context(installer: &Installer) -> PluginPlanningContext {
    PluginPlanningContext {
        app: installer.app.clone(),
        install_directory: installer.install.directory.user.clone().unwrap(),
        scope: SelectedScope::User,
        selected_components: Vec::new(),
        target: installer.target.clone(),
    }
}

#[test]
fn valid_empty_component_loads_and_runs() {
    let bundle = build_package(&empty_aot(), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    assert_eq!(executor.target(), &host_target());
    let installer = installer();
    let binding = installer.plugins[0].clone();
    let proposal = executor
        .plan(&binding, &context(&installer), &NeverCancelled)
        .unwrap();
    assert_eq!(proposal, PluginResourceProposal::default());
}

#[test]
fn configure_fixture_is_one_deterministic_generated_file_without_imports() {
    let bundle = build_package(&fixture("configure"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let mut context = context(&installer);
    context.selected_components = vec![ComponentId::new("core").unwrap()];
    let first = executor
        .plan(&installer.plugins[0], &context, &NeverCancelled)
        .unwrap();
    assert_eq!(first.resources.len(), 1);
    let PluginResource::GeneratedFile {
        destination,
        contents,
    } = &first.resources[0]
    else {
        panic!("configure fixture returned a non-file resource");
    };
    assert_eq!(destination, "${install}/plugin-config.txt");
    assert_eq!(
        contents,
        b"app id: com.example.runtime\ninstall directory: ${location.user_data}/Runtime\nselected components: core\n"
    );
    let second = executor
        .plan(&installer.plugins[0], &context, &NeverCancelled)
        .unwrap();
    assert_eq!(first, second);
}

#[test]
fn oversized_output_maps_to_a_typed_failure() {
    let bundle = build_package(&fixture("output"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert!(matches!(error, zup_plan::PluginFailure::OutputLimit { .. }));
}

#[test]
fn converts_all_resource_families_at_the_public_executor_seam() {
    let bundle = build_package(&fixture("all"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let proposal = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap();
    assert_eq!(proposal.resources.len(), 6);
    assert!(matches!(
        proposal.resources[0],
        PluginResource::GeneratedFile { .. }
    ));
    assert!(matches!(
        proposal.resources[1],
        PluginResource::Launcher { .. }
    ));
    assert!(matches!(
        proposal.resources[2],
        PluginResource::PathEntry { .. }
    ));
    assert!(matches!(
        proposal.resources[3],
        PluginResource::Service { .. }
    ));
    assert!(matches!(
        proposal.resources[4],
        PluginResource::Protocol { .. }
    ));
    assert!(matches!(
        proposal.resources[5],
        PluginResource::FileAssociation { .. }
    ));
}

#[test]
fn guest_rejection_maps_to_a_typed_failure() {
    let bundle = build_package(&fixture("rejected"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert!(matches!(
        error,
        zup_plan::PluginFailure::Rejected { ref code, ref message }
            if code == "bad" && message == "failure"
    ));
}

#[test]
fn cancellation_maps_to_a_typed_failure() {
    let bundle = build_package(&fixture("fuel"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let calls = AtomicUsize::new(0);
    let cancellation = || calls.fetch_add(1, Ordering::Relaxed) > 0;
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &cancellation)
        .unwrap_err();
    assert_eq!(error, zup_plan::PluginFailure::Cancelled);
}

#[test]
fn trap_maps_to_a_typed_failure() {
    let bundle = build_package(&fixture("trap"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert!(matches!(error, zup_plan::PluginFailure::Trap { .. }));
}

#[test]
fn fuel_exhaustion_maps_to_a_typed_failure() {
    let bundle = build_package(&fixture("fuel"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert_eq!(error, zup_plan::PluginFailure::FuelExhausted);
}

#[test]
fn memory_growth_rejection_maps_to_a_typed_failure() {
    let bundle = build_package(&fixture("memory"), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert_eq!(error, zup_plan::PluginFailure::MemoryLimit);
}

fn other_target() -> TargetTriple {
    let (architecture, suffix) = HOST_TARGET.split_once('-').unwrap();
    let other = if architecture == "aarch64" {
        "x86_64"
    } else {
        "aarch64"
    };
    TargetTriple::parse(format!("{other}-{suffix}")).unwrap()
}

#[test]
fn context_target_mismatch_is_rejected_before_invocation() {
    let bundle = build_package(&empty_aot(), &host_target());
    let mut executor = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap();
    let installer = installer();
    let mut context = context(&installer);
    context.target = TargetTriple::parse("arm64-pc-windows-msvc").unwrap();
    let error = executor
        .plan(&installer.plugins[0], &context, &NeverCancelled)
        .unwrap_err();
    assert!(matches!(
        error,
        zup_plan::PluginFailure::TargetMismatch { .. }
    ));
}

#[test]
fn target_mismatch_is_rejected_before_deserialize() {
    let target = other_target();
    let bundle = build_package(&empty_aot(), &target);
    assert_eq!(bundle.plan().installer.target, target);
    assert_eq!(bundle.plugin_artifacts()[0].target, target);
    let error = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap_err();
    assert!(matches!(error, LoadError::TargetMismatch { .. }));
}

#[test]
fn raw_wasm_is_rejected_before_deserialize() {
    let bundle = build_package(b"\0asm\x01\0\0\0", &host_target());
    let error = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap_err();
    assert!(matches!(error, LoadError::NotAComponent { .. }));
}

#[test]
fn bad_precompiled_bytes_are_rejected_before_deserialize() {
    let bundle = build_package(b"not a precompiled component", &host_target());
    let error = WasmtimePluginExecutor::load(bundle, &host_target()).unwrap_err();
    assert!(matches!(error, LoadError::NotAComponent { .. }));
}

#[test]
fn package_clone_loads_independently() {
    let aot = empty_aot();
    let package = build_package(&aot, &host_target());
    let clone = package.clone();
    let executor = WasmtimePluginExecutor::load(clone, &host_target()).unwrap();
    assert_eq!(executor.plugin_count(), 1);
    assert_eq!(
        package
            .plugin_aot(&PluginId::new(PLUGIN_ID).unwrap())
            .unwrap(),
        aot
    );
}

#[test]
fn loader_reports_package_errors_for_tampered_aot() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("runtime.zupbundle");
    let mut bytes = BundleWriter::encode(
        &plan(host_target()),
        &[artifact(&empty_aot(), &host_target())],
    )
    .unwrap();
    let metadata_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;
    bytes[60 + metadata_len] ^= 1;
    fs::write(&path, bytes).unwrap();
    let package = Package::open_unverified(path).unwrap();
    let error = WasmtimePluginExecutor::load(package, &host_target()).unwrap_err();
    assert!(matches!(error, LoadError::Package(_)));
}

#[test]
fn package_rejects_tampered_plugin_bytes_before_loading() {
    let mut package = BundleWriter::encode(
        &plan(host_target()),
        &[artifact(&empty_aot(), &host_target())],
    )
    .unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    package[60 + metadata_len] ^= 1;
    assert!(Package::parse(package).is_err());
}

fn assert_metadata_mutation_rejected(field: &str, value: serde_json::Value) {
    let mut package = BundleWriter::encode(
        &plan(host_target()),
        &[artifact(&empty_aot(), &host_target())],
    )
    .unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    metadata["plan"]["plugins"][0][field] = value;
    let metadata = serde_json::to_vec(&metadata).unwrap();
    package.splice(60..60 + metadata_len, metadata.clone());
    package[20..28].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    package[28..60].copy_from_slice(&Sha256::digest(&metadata));
    assert!(Package::parse(package).is_err());
}

#[test]
fn package_rejects_contract_metadata_mismatches_before_loading() {
    let zero_digest = serde_json::json!("0".repeat(64));
    for (field, value) in [
        ("wasmtime_version", serde_json::json!("48.0.0")),
        ("aot_format_version", serde_json::json!(2)),
        ("plugin_api_version", serde_json::json!("0.9.0")),
        ("wit_digest", zero_digest.clone()),
        ("engine_fingerprint", zero_digest),
    ] {
        assert_metadata_mutation_rejected(field, value);
    }
}
