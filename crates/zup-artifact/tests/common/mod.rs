//! A realistic two-architecture fixture shared by the artifact tests.
//!
//! The shape matters: most of an application is the same on both machines, some
//! is architecture-specific, and the shared part is what composition has to store
//! once. A fixture of only-different files would make deduplication look free, and
//! a fixture of only-identical files would make it look unnecessary.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zup_artifact::{DistributionVariant, MediaType};
use zup_bundle::CompiledPluginArtifact;
use zup_core::{
    App, AppId, Component, FileMapping, Frontend, Install, InstallDirectory, InstallScope,
    NonEmptyString, PluginBinding, PluginId, TargetProfileId, TargetTriple, Template, UpdateConfig,
};
use zup_plugin_contract::{AOT_FORMAT_VERSION, PLUGIN_API_VERSION, WASMTIME_VERSION};

/// One resolved target in the fixture.
pub struct FixtureTarget {
    pub profile: &'static str,
    pub target: &'static str,
    pub frontend: Frontend,
    /// Files this target has that the other one does not.
    pub exclusive: &'static [(&'static str, &'static str)],
    /// Files only this target installs, for a component declared per target.
    pub component: Option<&'static str>,
    pub plugin: Option<&'static str>,
}

pub const X64: FixtureTarget = FixtureTarget {
    profile: "windows-x64",
    target: "x86_64-pc-windows-msvc",
    frontend: Frontend::Gui,
    exclusive: &[
        ("bin/Acme.exe", "x64 machine code"),
        ("bin/acme-agent.exe", "x64 machine code"),
    ],
    component: None,
    plugin: None,
};

pub const ARM64: FixtureTarget = FixtureTarget {
    profile: "windows-arm64",
    target: "aarch64-pc-windows-msvc",
    frontend: Frontend::Gui,
    exclusive: &[
        ("bin/Acme.exe", "arm64 machine code"),
        ("bin/acme-agent.exe", "arm64 machine code"),
    ],
    component: None,
    plugin: Some("x64"),
};

/// Files both targets have, byte-identical. This is the content composition
/// must store once.
pub const SHARED: &[(&str, &str)] = &[
    (
        "assets/logo.png",
        "the same 4 KiB of pixels on every machine",
    ),
    (
        "assets/strings.json",
        "the same translations on every machine",
    ),
    (
        "runtime/framework.dat",
        "the same managed runtime on every machine",
    ),
    ("docs/license.txt", "the same license text on every machine"),
];

pub fn app() -> App {
    App {
        id: AppId::new("com.acme.desktop").unwrap(),
        name: NonEmptyString::new("Acme").unwrap(),
        version: semver::Version::parse("1.4.0").unwrap(),
        publisher: Some(NonEmptyString::new("Acme Inc.").unwrap()),
        main: Some(Template::parse("${install}/bin/Acme.exe").unwrap()),
        description: None,
    }
}

pub fn updates() -> UpdateConfig {
    UpdateConfig {
        repository: "https://releases.acme.test/acme".to_owned(),
        channel: "stable".to_owned(),
        trusted_root: b"zup trusted root bytes".to_vec(),
    }
}

pub fn install() -> Install {
    Install {
        scope: InstallScope::Either,
        directory: InstallDirectory {
            user: Some(Template::parse("${location.programs}/Acme").unwrap()),
            machine: Some(Template::parse("${location.programs}/Acme").unwrap()),
        },
        allow_directory_override: true,
    }
}

fn filler(seed: &str, target: u64) -> String {
    let mut out = String::new();
    while out.len() < 8192 {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(target.to_le_bytes());
        hasher.update(out.as_bytes());
        out.push_str(&zup_core::Sha256Digest::from_bytes(hasher.finalize().into()).to_hex());
    }
    out.truncate(8192);
    out
}

fn plugin_metadata(target: &str) -> zup_bundle::PluginArtifact {
    let mut aot = Vec::new();
    aot.extend_from_slice(filler("plugin-aot", 7).as_bytes());
    aot.extend_from_slice(target.as_bytes());
    let digest = zup_core::Sha256Digest::from_bytes(Sha256::digest(&aot).into());
    zup_bundle::PluginArtifact {
        plugin_id: PluginId::new("acme-plugins").unwrap(),
        source_size: 16,
        source_sha256: zup_core::Sha256Digest::from_bytes(Sha256::digest(b"source").into()),
        target: TargetTriple::parse(target).unwrap(),
        wasmtime_version: WASMTIME_VERSION.to_owned(),
        aot_format_version: AOT_FORMAT_VERSION,
        plugin_api_version: PLUGIN_API_VERSION.to_owned(),
        wit_digest: zup_core::Sha256Digest::from_bytes(zup_plugin_contract::wit_package_digest()),
        engine_fingerprint: zup_core::Sha256Digest::from_bytes(
            *zup_plugin_contract::engine_fingerprint(target).as_bytes(),
        ),
        aot_size: aot.len() as u64,
        aot_sha256: digest,
        blob: digest,
    }
}

fn compiled_plugin(target: &str) -> CompiledPluginArtifact {
    let mut aot = Vec::new();
    aot.extend_from_slice(filler("plugin-aot", 7).as_bytes());
    aot.extend_from_slice(target.as_bytes());
    CompiledPluginArtifact::new(plugin_metadata(target), aot).unwrap()
}

/// Build one target's plan and materialize its bytes under `root`.
pub fn build_target(root: impl AsRef<Path>, target: &FixtureTarget) -> DistributionVariant {
    let root = root.as_ref();
    std::fs::create_dir_all(root).unwrap();
    let resolved = zup_core::ResolvedTargetConfig {
        profile: TargetProfileId::new(target.profile).unwrap(),
        target: TargetTriple::parse(target.target).unwrap(),
        source: zup_core::Source::new(root.to_path_buf()).unwrap(),
        frontend: target.frontend,
        install: install(),
    };

    let mut files: Vec<(String, String, PathBuf)> = Vec::new();
    for (name, seed) in SHARED {
        files.push((
            (*name).to_owned(),
            filler(seed, 0),
            root.join("shared").join(name),
        ));
    }
    for (name, seed) in target.exclusive {
        files.push((
            (*name).to_owned(),
            filler(seed, 1),
            root.join("own").join(name),
        ));
    }

    let mut resolved_files = Vec::new();
    let mut total = 0u64;
    for (name, content, path) in &files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
        let size = content.len() as u64;
        total += size;
        resolved_files.push(zup_build::ResolvedFile {
            source: path.clone(),
            source_relative: zup_core::RelativePath::new(name.as_str()).unwrap(),
            destination: Template::parse(&format!("${{install}}/{name}")).unwrap(),
            size,
            sha256: zup_core::Sha256Digest::from_bytes(Sha256::digest(content.as_bytes()).into()),
            component: target
                .component
                .map(|component| zup_core::ComponentId::new(component).unwrap()),
            condition: None,
        });
    }

    let mut installer_files: Vec<FileMapping> = resolved_files
        .iter()
        .map(|file| FileMapping {
            source: "embedded".to_owned(),
            destination: file.destination.clone(),
            component: file.component.clone(),
            when: None,
            allow_empty: false,
        })
        .collect();
    installer_files.sort_by(|left, right| {
        left.destination
            .to_string()
            .cmp(&right.destination.to_string())
    });

    let plugins = target
        .plugin
        .map(|_| vec![compiled_plugin(target.target)])
        .unwrap_or_default();

    let installer = zup_core::Installer {
        app: app(),
        target: resolved.target.clone(),
        frontend: target.frontend,
        ui: None,
        updates: Some(updates()),
        install: install(),
        prerequisites: Vec::new(),
        components: vec![Component {
            id: zup_core::ComponentId::new("core").unwrap(),
            name: NonEmptyString::new("Application").unwrap(),
            description: None,
            required: true,
            default: true,
            requires: Vec::new(),
        }],
        plugins: plugins
            .iter()
            .map(|plugin| PluginBinding {
                id: plugin.metadata().plugin_id.clone(),
                component: None,
                when: None,
            })
            .collect(),
        files: installer_files,
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    };

    let plan = zup_build::TargetBuildPlan {
        installer,
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        files: resolved_files,
        total_size: total,
        prerequisite_size: 0,
    };

    // A stand-in for a native runtime template. The real build passes the actual
    // template executable; the fixture only needs distinct, verifiable bytes.
    let mut runtime = filler("runtime-image", 0).into_bytes();
    runtime.extend_from_slice(target.target.as_bytes());

    DistributionVariant::resolve(
        &resolved,
        &plan,
        &plugins,
        Some((MediaType::RUNTIME, runtime)),
    )
    .unwrap()
}
