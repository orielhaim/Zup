use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zup_artifact::{DistributionVariant, MediaType};
use zup_core::{
    App, AppId, Component, FileMapping, Frontend, Install, InstallDirectory, InstallScope,
    NonEmptyString, TargetProfileId, TargetTriple, Template, UpdateConfig,
};

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

fn install() -> Install {
    Install {
        scope: InstallScope::Either,
        directory: InstallDirectory {
            user: Some(Template::parse("${location.programs}/Acme").unwrap()),
            machine: Some(Template::parse("${location.programs}/Acme").unwrap()),
        },
        allow_directory_override: true,
    }
}

fn updates() -> UpdateConfig {
    UpdateConfig {
        repository: "https://releases.acme.test/acme".to_owned(),
        channel: "stable".to_owned(),
        trusted_root: b"zup trusted root bytes".to_vec(),
    }
}

fn filler(seed: &str, tag: u64) -> String {
    let mut out = String::new();
    while out.len() < 8192 {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(tag.to_le_bytes());
        hasher.update(out.as_bytes());
        out.push_str(&zup_core::Sha256Digest::from_bytes(hasher.finalize().into()).to_hex());
    }
    out.truncate(8192);
    out
}

const SHARED: &[(&str, &str)] = &[
    ("assets/logo.png", "the same pixels on every machine"),
    (
        "assets/strings.json",
        "the same translations on every machine",
    ),
    (
        "runtime/framework.dat",
        "the same managed runtime on every machine",
    ),
];

pub struct Fixture {
    pub variants: Vec<DistributionVariant>,

    #[allow(dead_code)]
    root: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let variants = vec![
            build(&root, "windows-x86", "i686-pc-windows-msvc", 3),
            build(&root, "windows-x64", "x86_64-pc-windows-msvc", 1),
            build(&root, "windows-arm64", "aarch64-pc-windows-msvc", 2),
        ];
        Self { variants, root }
    }

    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        self.root.path()
    }
}

fn build(root: &tempfile::TempDir, profile: &str, target: &str, tag: u64) -> DistributionVariant {
    let root = root.path().join(profile);
    std::fs::create_dir_all(&root).expect("target root");
    let resolved = zup_core::ResolvedTargetConfig {
        profile: TargetProfileId::new(profile).unwrap(),
        target: TargetTriple::parse(target).unwrap(),
        source: zup_core::Source::new(root.clone()).unwrap(),
        frontend: Frontend::Console,
        install: install(),
    };

    let mut files: Vec<(String, String, PathBuf)> = SHARED
        .iter()
        .map(|(name, seed)| {
            (
                (*name).to_owned(),
                filler(seed, 0),
                root.join("shared").join(name),
            )
        })
        .collect();
    for name in ["bin/Acme.exe", "bin/acme-agent.exe"] {
        files.push((
            name.to_owned(),
            filler(name, tag),
            root.join("own").join(name),
        ));
    }

    let mut resolved_files = Vec::new();
    let mut total = 0u64;
    for (name, content, path) in &files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("file parent");
        }
        std::fs::write(path, content).expect("fixture file");
        let size = content.len() as u64;
        total += size;
        resolved_files.push(zup_build::ResolvedFile {
            source: path.clone(),
            source_relative: zup_core::RelativePath::new(name.as_str()).unwrap(),
            destination: Template::parse(&format!("${{install}}/{name}")).unwrap(),
            size,
            sha256: zup_core::Sha256Digest::from_bytes(Sha256::digest(content.as_bytes()).into()),
            component: None,
            condition: None,
            executable: name.ends_with(".exe"),
        });
    }

    let mut installer_files: Vec<FileMapping> = resolved_files
        .iter()
        .map(|file| FileMapping {
            source: "embedded".to_owned(),
            destination: file.destination.clone(),
            component: None,
            when: None,
            allow_empty: false,
            executable: file.executable,
        })
        .collect();
    installer_files.sort_by(|left, right| {
        left.destination
            .to_string()
            .cmp(&right.destination.to_string())
    });

    let plan = zup_build::TargetBuildPlan {
        installer: zup_core::Installer {
            preset: None,
            app: app(),
            target: resolved.target.clone(),
            frontend: Frontend::Console,
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
                group: None,
            }],
            component_groups: Vec::new(),
            plugins: Vec::new(),
            files: installer_files,
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        files: resolved_files,
        ui_assets: Vec::new(),
        total_size: total,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
    };

    let mut runtime = filler("runtime-image", 0).into_bytes();
    runtime.extend_from_slice(target.as_bytes());
    DistributionVariant::resolve(&resolved, &plan, &[], &[(MediaType::RUNTIME, runtime)])
        .expect("the variant resolves")
}
