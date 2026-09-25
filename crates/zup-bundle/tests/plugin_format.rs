use std::{fs, path::Path};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{ResolvedPrerequisite, materialize};
use zup_bundle::{
    BundleError, BundleWriter, CompiledPluginArtifact, EmbeddedBundle, PluginArtifact,
};
use zup_core::{MAX_PLUGIN_ARTIFACTS, PluginBinding, PluginId, Sha256Digest};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, PluginEngine, WASMTIME_VERSION,
    engine_fingerprint, wit_package_digest,
};

fn project() -> (TempDir, zup_build::BuildPlan) {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"payload").unwrap();
    fs::write(root.path().join("plugins/one.wasm"), b"one").unwrap();
    fs::write(root.path().join("plugins/two.wasm"), b"two").unwrap();
    let source = r#"
schema = 1
[app]
id = "com.example.bundle-plugins"
name = "Bundle Plugins"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/BundlePlugins"
[[files]]
source = "**/*"
destination = "${install}"
[[plugins]]
id = "z-plugin"
source = "plugins/one.wasm"
[[plugins]]
id = "a-plugin"
source = "plugins/two.wasm"
"#;
    let manifest = zup_manifest::parse(source).unwrap();
    let installer = zup_manifest::parse_and_compile(source).unwrap();
    let plan = materialize(&root.path().join("zup.toml"), &manifest, installer).unwrap();
    (root, plan)
}

fn artifact(plan: &zup_build::BuildPlan, id: &str, aot: &[u8]) -> CompiledPluginArtifact {
    let resolved = plan
        .plugins
        .iter()
        .find(|plugin| plugin.id.as_str() == id)
        .unwrap();
    let digest = Sha256Digest::from_bytes(Sha256::digest(aot).into());
    CompiledPluginArtifact::new(
        PluginArtifact {
            plugin_id: resolved.id.clone(),
            source_size: resolved.size,
            source_sha256: resolved.sha256,
            target: HOST_TARGET.to_owned(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(
                *engine_fingerprint(HOST_TARGET).as_bytes(),
            ),
            aot_size: aot.len() as u64,
            aot_sha256: digest,
            blob: digest,
        },
        aot.to_vec(),
    )
    .unwrap()
}

fn synthetic_artifact(id: &str, bytes: &[u8]) -> CompiledPluginArtifact {
    let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    CompiledPluginArtifact::new(
        PluginArtifact {
            plugin_id: PluginId::new(id).unwrap(),
            source_size: bytes.len() as u64,
            source_sha256: digest,
            target: HOST_TARGET.to_owned(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(
                *engine_fingerprint(HOST_TARGET).as_bytes(),
            ),
            aot_size: bytes.len() as u64,
            aot_sha256: digest,
            blob: digest,
        },
        bytes.to_vec(),
    )
    .unwrap()
}

fn embed(root: &Path, package: &[u8]) -> std::path::PathBuf {
    let package_path = root.join("bundle.zupbundle");
    fs::write(&package_path, package).unwrap();
    let output = root.join("Setup.exe");
    zup_bundle::embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package_path)
        .unwrap();
    output
}

#[test]
fn schema_four_deduplicates_plugin_blobs_without_payload_entries() {
    let (root, plan) = project();
    let one = artifact(&plan, "z-plugin", b"same aot");
    let two = artifact(&plan, "a-plugin", b"same aot");
    let artifacts = vec![one.clone(), two.clone()];
    let package = BundleWriter::encode(&plan, &[two, one]).unwrap();
    assert_eq!(u32::from_le_bytes(package[8..12].try_into().unwrap()), 4);
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let metadata: Value = serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    assert_eq!(metadata["plan"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(metadata["plan"]["plugins"].as_array().unwrap().len(), 2);
    assert_eq!(metadata["blobs"].as_array().unwrap().len(), 2);
    assert_eq!(BundleWriter::encode(&plan, &artifacts).unwrap(), package);
    let written = root.path().join("written.zupbundle");
    assert_eq!(
        BundleWriter::write_file(&plan, &artifacts, &written).unwrap(),
        package.len() as u64
    );
    assert_eq!(fs::read(&written).unwrap(), package);
    let output = embed(root.path(), &package);
    let bundle = EmbeddedBundle::open(&output).unwrap();
    assert_eq!(bundle.plugin_artifacts().len(), 2);
    let id = PluginId::new("a-plugin").unwrap();
    assert_eq!(bundle.plugin_aot(&id).unwrap(), b"same aot");
    assert!(bundle.build_plan().unwrap().plugins.is_empty());
    assert_eq!(bundle.plan().installer.plugins.len(), 2);
}

#[test]
fn artifact_bytes_cannot_disagree_with_metadata() {
    let (_root, plan) = project();
    let bytes = b"aot";
    let compiled = artifact(&plan, "z-plugin", bytes);
    let metadata = compiled.metadata().clone();
    assert!(CompiledPluginArtifact::new(metadata.clone(), b"wrong".to_vec()).is_err());
    let mut wrong_size = metadata;
    wrong_size.aot_size += 1;
    assert!(CompiledPluginArtifact::new(wrong_size, bytes.to_vec()).is_err());
}

#[test]
fn malformed_plugin_metadata_is_rejected_on_open() {
    let (root, plan) = project();
    let compiled = artifact(&plan, "z-plugin", b"aot");
    let second = artifact(&plan, "a-plugin", b"other aot");
    let mut package = BundleWriter::encode(&plan, &[compiled, second]).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let mut value: Value = serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    value["plan"]["plugins"][0]["aot_size"] = Value::from(99);
    let metadata = serde_json::to_vec(&value).unwrap();
    package.splice(60..60 + metadata_len, metadata.clone());
    package[20..28].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    let hash = Sha256::digest(&metadata);
    package[28..60].copy_from_slice(&hash);
    let package_path = root.path().join("malformed.zupbundle");
    fs::write(&package_path, &package).unwrap();
    let output = root.path().join("Malformed.exe");
    assert!(
        zup_bundle::embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package_path,)
            .is_err()
    );
}

#[test]
fn plugin_descriptor_order_is_strict() {
    let (root, plan) = project();
    let first = artifact(&plan, "z-plugin", b"aot");
    let second = artifact(&plan, "a-plugin", b"other");
    let mut package = BundleWriter::encode(&plan, &[first, second]).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let mut value: Value = serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    value["plan"]["plugins"].as_array_mut().unwrap().swap(0, 1);
    let metadata = serde_json::to_vec(&value).unwrap();
    package.splice(60..60 + metadata_len, metadata.clone());
    package[20..28].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    package[28..60].copy_from_slice(&Sha256::digest(&metadata));
    let package_path = root.path().join("unordered.zupbundle");
    fs::write(&package_path, &package).unwrap();
    let output = root.path().join("Unordered.exe");
    assert!(
        zup_bundle::embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package_path,)
            .is_err()
    );
}

#[test]
fn tampered_plugin_blob_is_rejected_on_open() {
    let (root, plan) = project();
    let compiled = artifact(&plan, "z-plugin", b"aot");
    let second = artifact(&plan, "a-plugin", b"other aot");
    let mut package = BundleWriter::encode(&plan, &[compiled, second]).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    package[60 + metadata_len] ^= 1;
    let output = embed(root.path(), &package);
    assert!(EmbeddedBundle::open(output).is_err());
}

#[test]
fn cross_target_metadata_is_accepted() {
    let (_root, plan) = project();
    let compiled = artifact(&plan, "z-plugin", b"aot");
    let mut metadata = compiled.metadata().clone();
    metadata.target = "aarch64-pc-windows-msvc".to_owned();
    metadata.engine_fingerprint = Sha256Digest::from_bytes(
        *zup_plugin_contract::engine_fingerprint(&metadata.target).as_bytes(),
    );
    assert!(CompiledPluginArtifact::new(metadata, b"aot".to_vec()).is_ok());
}

#[test]
fn plugin_target_is_checked_by_contract() {
    let (_root, plan) = project();
    let compiled = artifact(&plan, "z-plugin", b"aot");
    let mut metadata = compiled.metadata().clone();
    metadata.target = "not a target".to_owned();
    assert!(CompiledPluginArtifact::new(metadata, b"aot".to_vec()).is_err());
    let _ = PluginEngine::new(HOST_TARGET).unwrap();
}

#[test]
fn rejects_more_than_the_documented_plugin_descriptor_limit() {
    let (_root, mut plan) = project();
    plan.plugins.clear();
    plan.installer.plugins = (0..=MAX_PLUGIN_ARTIFACTS)
        .map(|index| PluginBinding {
            id: PluginId::new(format!("plugin-{index}")).unwrap(),
            component: None,
            when: None,
        })
        .collect();
    let artifacts = plan
        .installer
        .plugins
        .iter()
        .map(|binding| synthetic_artifact(binding.id.as_str(), b"aot"))
        .collect::<Vec<_>>();

    let error = BundleWriter::encode(&plan, &artifacts).unwrap_err();
    assert!(matches!(
        error,
        BundleError::TooManyPluginArtifacts {
            count,
            limit: MAX_PLUGIN_ARTIFACTS,
        } if count == MAX_PLUGIN_ARTIFACTS + 1
    ));
}

#[cfg(windows)]
#[test]
fn rejects_aggregate_aot_overflow_while_parsing_metadata() {
    let (root, mut plan) = project();
    plan.plugins.clear();
    plan.installer.plugins = (0..5)
        .map(|index| PluginBinding {
            id: PluginId::new(format!("plugin-{index}")).unwrap(),
            component: None,
            when: None,
        })
        .collect();
    let artifacts = plan
        .installer
        .plugins
        .iter()
        .map(|binding| synthetic_artifact(binding.id.as_str(), binding.id.as_str().as_bytes()))
        .collect::<Vec<_>>();
    let mut package = BundleWriter::encode(&plan, &artifacts).unwrap();
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let mut value: Value = serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    for artifact in value["plan"]["plugins"].as_array_mut().unwrap() {
        artifact["aot_size"] = Value::from(zup_plugin_contract::MAX_AOT_BYTES as u64);
    }
    let metadata = serde_json::to_vec(&value).unwrap();
    package.splice(60..60 + metadata_len, metadata.clone());
    package[20..28].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    package[28..60].copy_from_slice(&Sha256::digest(&metadata));
    let package_path = root.path().join("aggregate-overflow.zupbundle");
    fs::write(&package_path, &package).unwrap();
    let output = root.path().join("AggregateOverflow.exe");

    let error =
        zup_bundle::embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package_path)
            .unwrap_err();
    assert!(matches!(error, BundleError::PluginAotTooLarge { .. }));
}

#[test]
fn embedded_prerequisite_bytes_roundtrip_in_bundle() {
    let (root, mut plan) = project();
    plan.plugins.clear();
    plan.installer.plugins.clear();
    let bytes = b"runtime payload";
    let source = root.path().join("runtime.exe");
    fs::write(&source, bytes).unwrap();
    let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    let id = zup_core::PrerequisiteId::new("runtime").unwrap();
    plan.installer.prerequisites.push(zup_core::Prerequisite {
        id: id.clone(),
        name: zup_core::NonEmptyString::new("Runtime").unwrap(),
        description: None,
        component: None,
        when: None,
        target: zup_core::PrerequisiteArchitecture::Current,
        detector: zup_core::PrerequisiteDetector::VisualCppV14 { version: None },
        package: zup_core::PrerequisitePackage::Embedded {
            path: zup_core::RelativePath::new("runtime.exe").unwrap(),
            sha256: digest,
            size: bytes.len() as u64,
        },
        installer: Default::default(),
    });
    plan.prerequisites.push(ResolvedPrerequisite {
        id,
        source,
        source_relative: zup_core::RelativePath::new("runtime.exe").unwrap(),
        size: bytes.len() as u64,
        sha256: digest,
    });
    plan.prerequisite_size = bytes.len() as u64;
    let package = BundleWriter::encode(&plan, &[]).unwrap();
    let package_path = root.path().join("prerequisite.zupbundle");
    fs::write(&package_path, &package).unwrap();
    let output = root.path().join("PrerequisiteSetup.exe");
    zup_bundle::embed_bundle_file(&std::env::current_exe().unwrap(), &output, &package_path)
        .unwrap();
    let bundle = EmbeddedBundle::open(&output).unwrap();
    let prerequisite_id = zup_core::PrerequisiteId::new("runtime").unwrap();
    assert_eq!(bundle.prerequisite_bytes(&prerequisite_id).unwrap(), bytes);
    assert_eq!(bundle.build_plan().unwrap().prerequisites[0].sha256, digest);
}
