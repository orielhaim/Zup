use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};
use zup_build::materialize;
use zup_core::Sha256Digest;
use zup_plugin_build::{PluginBuildError, compile_plugins};
use zup_plugin_contract::{ContractError, HOST_TARGET, PluginEngine};

const WIT: &str = include_str!("../../../wit/zup-plugin.wit");

fn component_for_wit() -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", WIT).unwrap();
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

fn project() -> (TempDir, zup_build::BuildPlan) {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    fs::write(root.path().join("dist/app.bin"), b"app").unwrap();
    fs::write(root.path().join("plugins/helper.wasm"), component_for_wit()).unwrap();
    let source = r#"
schema = 1
[app]
id = "com.example.plugin-build"
name = "Plugin Build"
version = "1.0.0"
[source]
directory = "dist"
[install]
scope = "user"
[install.directory]
user = "${known.local_app_data}/PluginBuild"
[[files]]
source = "**/*"
destination = "${install}"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#;
    let manifest = zup_manifest::parse(source).unwrap();
    let installer = zup_manifest::parse_and_compile(source).unwrap();
    let plan = materialize(&root.path().join("zup.toml"), &manifest, installer).unwrap();
    (root, plan)
}

fn update_source(plan: &mut zup_build::BuildPlan, path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    plan.plugins[0].size = bytes.len() as u64;
    plan.plugins[0].sha256 = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
}

#[test]
fn compiles_and_verifies_a_valid_component() {
    let (root, plan) = project();
    let artifacts = compile_plugins(&plan, HOST_TARGET).unwrap();
    assert_eq!(artifacts.len(), 1);
    let metadata = artifacts[0].metadata();
    assert_eq!(metadata.target, HOST_TARGET);
    assert_eq!(metadata.source_sha256, plan.plugins[0].sha256);
    assert_eq!(metadata.blob, metadata.aot_sha256);
    assert!(!artifacts[0].bytes().is_empty());
    let runtime = PluginEngine::new(HOST_TARGET).unwrap();
    let component = runtime
        .compile_component(&fs::read(root.path().join("plugins/helper.wasm")).unwrap())
        .unwrap();
    assert_eq!(component.fingerprint(), runtime.fingerprint());
}

#[test]
fn rejects_unsupported_imports() {
    let (root, mut plan) = project();
    let component = wat::parse_str(r#"(component (import "env" (func)))"#).unwrap();
    update_source(
        &mut plan,
        &root.path().join("plugins/helper.wasm"),
        &component,
    );
    let error = compile_plugins(&plan, HOST_TARGET).unwrap_err();
    assert!(matches!(
        error,
        PluginBuildError::Contract(ContractError::UnexpectedImport { .. })
    ));
}

#[test]
fn rejects_malformed_components() {
    let (root, mut plan) = project();
    update_source(
        &mut plan,
        &root.path().join("plugins/helper.wasm"),
        b"not a component",
    );
    let error = compile_plugins(&plan, HOST_TARGET).unwrap_err();
    assert!(matches!(
        error,
        PluginBuildError::Contract(ContractError::Compilation { .. })
    ));
}

#[test]
fn rejects_core_modules() {
    let (root, mut plan) = project();
    let module = wat::parse_str("(module)").unwrap();
    update_source(&mut plan, &root.path().join("plugins/helper.wasm"), &module);
    let error = compile_plugins(&plan, HOST_TARGET).unwrap_err();
    assert!(matches!(
        error,
        PluginBuildError::Contract(ContractError::CoreModule)
    ));
}

#[test]
fn compiles_for_an_explicit_cross_target() {
    let (_root, plan) = project();
    let artifacts = compile_plugins(&plan, "aarch64-pc-windows-msvc").unwrap();
    assert_eq!(artifacts[0].metadata().target, "aarch64-pc-windows-msvc");
}

#[test]
fn rejects_oversized_sources_before_compilation() {
    let (root, mut plan) = project();
    let oversized = vec![0; zup_build::MAX_PLUGIN_SOURCE_BYTES as usize + 1];
    update_source(
        &mut plan,
        &root.path().join("plugins/helper.wasm"),
        &oversized,
    );
    let error = compile_plugins(&plan, HOST_TARGET).unwrap_err();
    assert!(matches!(error, PluginBuildError::SourceTooLarge { .. }));
}

#[test]
fn rejects_unsupported_targets() {
    let (_root, plan) = project();
    let error = compile_plugins(&plan, "not a target").unwrap_err();
    assert!(matches!(error, PluginBuildError::Engine(_)));
}
