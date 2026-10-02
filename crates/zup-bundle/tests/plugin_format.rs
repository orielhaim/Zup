use std::fs;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{ResolvedPrerequisite, TargetBuildPlan, materialize};
use zup_bundle::{
    BundleWriter, CompiledPluginArtifact, PACKAGE_SCHEMA, Package, PackageError, PluginArtifact,
};
use zup_core::{MAX_PLUGIN_ARTIFACTS, PluginBinding, PluginId, Sha256Digest, TargetTriple};
use zup_manifest::TargetOverrides;
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, HOST_TARGET, PLUGIN_API_VERSION, WASMTIME_VERSION, engine_fingerprint,
    wit_package_digest,
};

fn project() -> (TempDir, TargetBuildPlan) {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"payload").unwrap();
    fs::write(root.path().join("plugins/one.wasm"), b"one").unwrap();
    fs::write(root.path().join("plugins/two.wasm"), b"two").unwrap();
    let source = format!(
        r#"
schema = 1
[app]
id = "com.example.bundle-plugins"
name = "Bundle Plugins"
version = "1.0.0"
[build]
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "user"
[install.directory]
user = "${{location.user_data}}/BundlePlugins"
[[files]]
source = "**/*"
destination = "${{install}}"
[[plugins]]
id = "z-plugin"
source = "plugins/one.wasm"
[[plugins]]
id = "a-plugin"
source = "plugins/two.wasm"
"#,
        target = HOST_TARGET
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source, "default").unwrap();
    let config = zup_manifest::select_targets(&manifest, &["default"], &TargetOverrides::default())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut plan = materialize(
        &root.path().join("zup.toml"),
        &manifest,
        vec![(config, installer)],
        zup_build::Writes::None,
    )
    .unwrap();
    (root, plan.targets.pop().unwrap())
}

fn artifact(plan: &TargetBuildPlan, id: &str, aot: &[u8]) -> CompiledPluginArtifact {
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
            target: plan.installer.target.clone(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(
                *engine_fingerprint(plan.installer.target.as_str()).as_bytes(),
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
            target: TargetTriple::parse(HOST_TARGET).unwrap(),
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

fn rewrite_metadata(package: &mut Vec<u8>, mutate: impl FnOnce(&mut Value)) {
    let metadata_len = u64::from_le_bytes(package[20..28].try_into().unwrap()) as usize;
    let mut value: Value = serde_json::from_slice(&package[60..60 + metadata_len]).unwrap();
    mutate(&mut value);
    let metadata = serde_json::to_vec(&value).unwrap();
    package.splice(60..60 + metadata_len, metadata.clone());
    package[20..28].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    package[28..60].copy_from_slice(&Sha256::digest(&metadata));
}

#[test]
fn schema_one_deduplicates_plugin_blobs_and_round_trips() {
    let (root, plan) = project();
    let one = artifact(&plan, "z-plugin", b"same aot");
    let two = artifact(&plan, "a-plugin", b"same aot");
    let artifacts = vec![one.clone(), two.clone()];
    let package_bytes = BundleWriter::encode(&plan, &[two, one]).unwrap();
    assert_eq!(
        u32::from_le_bytes(package_bytes[8..12].try_into().unwrap()),
        PACKAGE_SCHEMA
    );
    let package = Package::parse(&package_bytes).unwrap();
    assert_eq!(package.plan().entries.len(), 1);
    assert_eq!(package.plan().plugins.len(), 2);
    assert_eq!(package.index_info().blob_count(), 2);
    assert_eq!(
        BundleWriter::encode(&plan, &artifacts).unwrap(),
        package_bytes
    );
    let written = root.path().join("written.zup");
    assert_eq!(
        BundleWriter::write_file(&plan, &artifacts, &written).unwrap(),
        package_bytes.len() as u64
    );
    assert_eq!(fs::read(&written).unwrap(), package_bytes);
    let id = PluginId::new("a-plugin").unwrap();
    assert_eq!(package.plugin_aot(&id).unwrap(), b"same aot");
    assert_eq!(package.build_plan().unwrap().targets.len(), 1);
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

/// The plugin table in a package's metadata is the reader's only map from a plugin id to
/// a blob, so a hand-edited one is refused: a descriptor that disagrees with its own
/// recorded size, and a table whose order no longer matches the ids it is keyed by.
#[test]
fn plugin_metadata_that_disagrees_with_itself_is_refused() {
    let (_root, plan) = project();
    let first = artifact(&plan, "z-plugin", b"aot");
    let second = artifact(&plan, "a-plugin", b"other aot");

    let mut wrong_size = BundleWriter::encode(&plan, &[first.clone(), second.clone()]).unwrap();
    rewrite_metadata(&mut wrong_size, |value| {
        value["plan"]["plugins"][0]["aot_size"] = Value::from(99);
    });
    assert!(Package::parse(wrong_size).is_err());

    let mut reordered = BundleWriter::encode(&plan, &[first, second]).unwrap();
    rewrite_metadata(&mut reordered, |value| {
        value["plan"]["plugins"].as_array_mut().unwrap().swap(0, 1);
    });
    assert!(Package::parse(reordered).is_err());
}

fn other_target() -> TargetTriple {
    let host = TargetTriple::parse(HOST_TARGET).unwrap();
    if host.as_str() == "aarch64-unknown-linux-gnu" {
        TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap()
    } else {
        TargetTriple::parse("aarch64-unknown-linux-gnu").unwrap()
    }
}

#[test]
fn plugin_target_must_match_target_plan() {
    let (_root, plan) = project();
    let other = other_target();
    let mut first = artifact(&plan, "z-plugin", b"aot");
    let mut second = artifact(&plan, "a-plugin", b"other aot");
    for artifact in [&mut first, &mut second] {
        let mut metadata = artifact.metadata().clone();
        metadata.target = other.clone();
        metadata.engine_fingerprint = Sha256Digest::from_bytes(
            *zup_plugin_contract::engine_fingerprint(other.as_str()).as_bytes(),
        );
        let bytes = artifact.bytes().to_vec();
        *artifact = CompiledPluginArtifact::new(metadata, bytes).unwrap();
    }
    let error = BundleWriter::encode(&plan, &[first, second]).unwrap_err();
    assert!(matches!(
        error,
        PackageError::TargetMismatch { expected, found }
            if expected == plan.installer.target && found == other
    ));
}

#[test]
fn persisted_plugin_target_must_match_installer_target() {
    let (_root, plan) = project();
    let first = artifact(&plan, "z-plugin", b"aot");
    let second = artifact(&plan, "a-plugin", b"other aot");
    let mut package = BundleWriter::encode(&plan, &[first, second]).unwrap();
    rewrite_metadata(&mut package, |value| {
        value["plan"]["plugins"][0]["target"] = Value::from(other_target().to_string());
    });
    assert!(matches!(
        Package::parse(package),
        Err(PackageError::TargetMismatch { .. })
    ));
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
        PackageError::TooManyPluginArtifacts {
            count,
            limit: MAX_PLUGIN_ARTIFACTS,
        } if count == MAX_PLUGIN_ARTIFACTS + 1
    ));
}

#[test]
fn rejects_aggregate_aot_overflow_while_parsing_metadata() {
    let (_root, mut plan) = project();
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
    rewrite_metadata(&mut package, |value| {
        for artifact in value["plan"]["plugins"].as_array_mut().unwrap() {
            artifact["aot_size"] = Value::from(zup_plugin_contract::MAX_AOT_BYTES as u64);
        }
    });
    assert!(matches!(
        Package::parse(package),
        Err(PackageError::PluginAotTooLarge { .. })
    ));
}

#[test]
fn embedded_prerequisite_bytes_round_trip_in_package() {
    let (root, mut plan) = project();
    plan.plugins.clear();
    plan.installer.plugins.clear();
    let bytes = b"runtime payload";
    let source = root.path().join("runtime.bin");
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
        requirement: zup_core::PrerequisiteRequirement::Runtime(zup_core::Runtime {
            id: zup_core::RuntimeRequirementId::new("windows.vc.v14").unwrap(),
            version: None,
        }),
        package: zup_core::PrerequisitePackage::Embedded {
            path: zup_core::RelativePath::new("runtime.bin").unwrap(),
            sha256: digest,
            size: bytes.len() as u64,
        },
        installer: Default::default(),
    });
    plan.prerequisites.push(ResolvedPrerequisite {
        id,
        source,
        source_relative: zup_core::RelativePath::new("runtime.bin").unwrap(),
        size: bytes.len() as u64,
        sha256: digest,
    });
    plan.prerequisite_size = bytes.len() as u64;
    let encoded = BundleWriter::encode(&plan, &[]).unwrap();
    let package = Package::parse(&encoded).unwrap();
    let package_path = root.path().join("prerequisite.zup");
    assert_eq!(
        BundleWriter::write_file(&plan, &[], &package_path).unwrap(),
        encoded.len() as u64
    );
    let opened = Package::open(&package_path).unwrap();
    let prerequisite_id = zup_core::PrerequisiteId::new("runtime").unwrap();
    assert_eq!(package.prerequisite_bytes(&prerequisite_id).unwrap(), bytes);
    assert_eq!(opened.prerequisite_bytes(&prerequisite_id).unwrap(), bytes);
    assert_eq!(
        package.build_plan().unwrap().targets[0].prerequisites[0].sha256,
        digest
    );
}
