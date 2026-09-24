#![cfg(windows)]

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use semver::Version;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::BuildPlan;
use zup_bundle::{BundleWriter, CompiledPluginArtifact, EmbeddedBundle, PluginArtifact};
use zup_core::{
    App, AppId, ComponentId, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
    PluginBinding, PluginId, SelectedScope, Sha256Digest, Template,
};
use zup_plan::{
    NeverCancelled, PluginArchitecture, PluginExecutor, PluginHostFacts, PluginOperatingSystem,
    PluginPlanningContext, PluginResource, PluginResourceProposal,
};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, WASMTIME_VERSION, engine_fingerprint,
    wit_package_digest,
};
use zup_plugin_runtime::{LoadError, WasmtimePluginExecutor};

const PLUGIN_ID: &str = "helper";

fn fixture(name: &str) -> Vec<u8> {
    let encoded = match name {
        "empty" => include_str!("fixtures/empty-plugin.aot.b64"),
        "trap" => include_str!("fixtures/trap-plugin.aot.b64"),
        "fuel" => include_str!("fixtures/fuel-plugin.aot.b64"),
        "memory" => include_str!("fixtures/memory-plugin.aot.b64"),
        "rejected" => include_str!("fixtures/rejected-plugin.aot.b64"),
        "output" => include_str!("fixtures/output-limit-plugin.aot.b64"),
        "all" => include_str!("fixtures/all-resources-plugin.aot.b64"),
        "configure" => include_str!("fixtures/configure-plugin.aot.b64"),
        _ => panic!("unknown fixture"),
    };
    base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .unwrap()
}

fn empty_aot() -> Vec<u8> {
    fixture("empty")
}

fn installer() -> Installer {
    Installer {
        ui: None,
        app: App {
            id: AppId::new("com.example.runtime").unwrap(),
            name: NonEmptyString::new("Runtime").unwrap(),
            version: Version::parse("1.0.0").unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        updates: None,
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse("${known.local_app_data}/Runtime").unwrap()),
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
        shortcuts: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_types: Vec::new(),
    }
}

fn plan() -> BuildPlan {
    BuildPlan {
        installer: installer(),
        plugins: Vec::new(),
        files: Vec::new(),
        total_size: 0,
    }
}

fn artifact(bytes: &[u8], target: &str) -> CompiledPluginArtifact {
    let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    CompiledPluginArtifact::new(
        PluginArtifact {
            plugin_id: PluginId::new(PLUGIN_ID).unwrap(),
            source_size: bytes.len() as u64,
            source_sha256: digest,
            target: target.to_owned(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(*engine_fingerprint(target).as_bytes()),
            aot_size: bytes.len() as u64,
            aot_sha256: digest,
            blob: digest,
        },
        bytes.to_vec(),
    )
    .unwrap()
}

fn open_bundle(bytes: &[u8], target: &str) -> (TempDir, EmbeddedBundle) {
    let temp = TempDir::new().unwrap();
    let package_path = temp.path().join("runtime.zupbundle");
    let package = BundleWriter::encode(&plan(), &[artifact(bytes, target)]).unwrap();
    fs::write(&package_path, package).unwrap();
    let executable = temp.path().join("Runtime.exe");
    zup_bundle::embed_bundle_file(&current_test_executable(), &executable, &package_path).unwrap();
    let bundle = EmbeddedBundle::open(&executable).unwrap();
    (temp, bundle)
}

fn current_test_executable() -> std::path::PathBuf {
    std::env::current_exe().unwrap()
}

fn context(installer: &Installer) -> PluginPlanningContext {
    PluginPlanningContext {
        app: installer.app.clone(),
        install_directory: installer.install.directory.user.clone().unwrap(),
        scope: SelectedScope::User,
        selected_components: Vec::new(),
        host: PluginHostFacts::new(PluginOperatingSystem::Windows, PluginArchitecture::X86_64),
    }
}

#[test]
fn valid_empty_component_loads_and_runs() {
    let (_temp, bundle) = open_bundle(&empty_aot(), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
    let installer = installer();
    let binding = installer.plugins[0].clone();
    let proposal = executor
        .plan(&binding, &context(&installer), &NeverCancelled)
        .unwrap();
    assert_eq!(proposal, PluginResourceProposal::default());
}

#[cfg(target_arch = "x86_64")]
#[test]
fn configure_fixture_is_one_deterministic_generated_file_without_imports() {
    let (_temp, bundle) = open_bundle(&fixture("configure"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
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
        b"app id: com.example.runtime\ninstall directory: ${known.local_app_data}/Runtime\nselected components: core\n"
    );
    let second = executor
        .plan(&installer.plugins[0], &context, &NeverCancelled)
        .unwrap();
    assert_eq!(first, second);
}

#[test]
fn oversized_output_maps_to_a_typed_failure() {
    let (_temp, bundle) = open_bundle(&fixture("output"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert!(matches!(error, zup_plan::PluginFailure::OutputLimit { .. }));
}

#[test]
fn converts_all_resource_families_at_the_public_executor_seam() {
    let (_temp, bundle) = open_bundle(&fixture("all"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
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
        PluginResource::Shortcut { .. }
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
        PluginResource::FileType { .. }
    ));
}

#[test]
fn guest_rejection_maps_to_a_typed_failure() {
    let (_temp, bundle) = open_bundle(&fixture("rejected"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
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
    let (_temp, bundle) = open_bundle(&fixture("fuel"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
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
    let (_temp, bundle) = open_bundle(&fixture("trap"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert!(matches!(error, zup_plan::PluginFailure::Trap { .. }));
}

#[test]
fn fuel_exhaustion_maps_to_a_typed_failure() {
    let (_temp, bundle) = open_bundle(&fixture("fuel"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert_eq!(error, zup_plan::PluginFailure::FuelExhausted);
}

#[test]
fn memory_growth_rejection_maps_to_a_typed_failure() {
    let (_temp, bundle) = open_bundle(&fixture("memory"), HOST_TARGET);
    let mut executor = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap();
    let installer = installer();
    let error = executor
        .plan(&installer.plugins[0], &context(&installer), &NeverCancelled)
        .unwrap_err();
    assert_eq!(error, zup_plan::PluginFailure::MemoryLimit);
}

#[test]
fn target_mismatch_is_rejected_before_deserialize() {
    let (_temp, bundle) = open_bundle(&empty_aot(), "aarch64-pc-windows-msvc");
    let error = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap_err();
    assert!(matches!(error, LoadError::TargetMismatch { .. }));
}

#[test]
fn raw_wasm_is_rejected_before_deserialize() {
    let (_temp, bundle) = open_bundle(b"\0asm\x01\0\0\0", HOST_TARGET);
    let error = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap_err();
    assert!(matches!(error, LoadError::NotAComponent { .. }));
}

#[test]
fn bad_precompiled_bytes_are_rejected_before_deserialize() {
    let (_temp, bundle) = open_bundle(b"not a precompiled component", HOST_TARGET);
    let error = WasmtimePluginExecutor::load(bundle, HOST_TARGET).unwrap_err();
    assert!(matches!(error, LoadError::NotAComponent { .. }));
}

#[test]
fn bundle_rejects_tampered_plugin_bytes_before_loading() {
    let temp = TempDir::new().unwrap();
    let package_path = temp.path().join("runtime.zupbundle");
    let mut package =
        BundleWriter::encode(&plan(), &[artifact(&empty_aot(), HOST_TARGET)]).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    package[60 + metadata_len] ^= 1;
    fs::write(&package_path, package).unwrap();
    let executable = temp.path().join("Runtime.exe");
    zup_bundle::embed_bundle_file(&current_test_executable(), &executable, &package_path).unwrap();
    assert!(EmbeddedBundle::open(&executable).is_err());
}

fn assert_metadata_mutation_rejected(field: &str, value: serde_json::Value) {
    let temp = TempDir::new().unwrap();
    let package_path = temp.path().join("runtime.zupbundle");
    let mut package =
        BundleWriter::encode(&plan(), &[artifact(&empty_aot(), HOST_TARGET)]).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    metadata["plan"]["plugins"][0][field] = value;
    let metadata = serde_json::to_vec(&metadata).unwrap();
    package.splice(60..60 + metadata_len, metadata.clone());
    package[20..28].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    package[28..60].copy_from_slice(&Sha256::digest(&metadata));
    fs::write(&package_path, package).unwrap();
    let executable = temp.path().join("Runtime.exe");
    assert!(
        zup_bundle::embed_bundle_file(&current_test_executable(), &executable, &package_path)
            .is_err()
    );
}

#[test]
fn bundle_rejects_contract_metadata_mismatches_before_loading() {
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
