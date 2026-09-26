#![allow(dead_code)]

use semver::Version;
use zup_bootstrap::{BootstrapKey, BootstrapOperation, BootstrapPlan};
use zup_core::{
    AppId, PrerequisiteArchitecture, PrerequisiteId, PrerequisiteInstaller, PrerequisitePackage,
    RelativePath, Runtime, RuntimeRequirementId, SelectedScope, Sha256Digest, TargetTriple,
};

pub fn digest(bytes: &[u8]) -> Sha256Digest {
    zup_core::hash_reader(bytes).unwrap().1
}

pub fn plan(package: PrerequisitePackage) -> BootstrapPlan {
    BootstrapPlan::new(
        BootstrapKey {
            app_id: AppId::new("com.example.bootstrap").unwrap(),
            app_version: Version::new(1, 0, 0),
            scope: SelectedScope::User,
            target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        },
        vec![BootstrapOperation {
            id: PrerequisiteId::new("runtime").unwrap(),
            name: "Runtime".into(),
            target: PrerequisiteArchitecture::Current,
            requirement: zup_core::PrerequisiteRequirement::Runtime(Runtime {
                id: RuntimeRequirementId::new("example.runtime").unwrap(),
                version: None,
            }),
            package,
            installer: PrerequisiteInstaller::default(),
        }],
    )
    .unwrap()
}

pub fn embedded(bytes: &[u8]) -> PrerequisitePackage {
    PrerequisitePackage::Embedded {
        path: RelativePath::new("runtime.exe").unwrap(),
        sha256: digest(bytes),
        size: bytes.len() as u64,
    }
}
