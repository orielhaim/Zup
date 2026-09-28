use semver::VersionReq;
use zup_core::{
    InstalledPackage, InstalledPackageId, PrerequisiteId, PrerequisiteRequirement, Runtime,
    RuntimeRequirementId, Template,
};

/// A prerequisite id ends up in a command line, so anything that could be read
/// as a path, an argument separator, or a flag is refused.
#[test]
fn prerequisite_id_rejects_path_and_argument_characters() {
    assert!(PrerequisiteId::new("vc-x64").is_ok());
    assert!(PrerequisiteId::new("../vc").is_err());
    assert!(PrerequisiteId::new("vc x64").is_err());
    assert!(PrerequisiteId::new("vc;calc").is_err());
}

#[test]
fn runtime_requirement_ids_are_namespaced_and_opaque() {
    assert!(RuntimeRequirementId::new("windows.vc.v14").is_ok());
    assert!(RuntimeRequirementId::new("com.example.runtime_2").is_ok());
    assert!(RuntimeRequirementId::new("").is_err());
    assert!(RuntimeRequirementId::new("Windows.Vc").is_err());
    assert!(RuntimeRequirementId::new("windows..vc").is_err());
    assert!(RuntimeRequirementId::new("windows.vc.").is_err());
    assert!(RuntimeRequirementId::new("windows.vc runtime").is_err());
    assert!(RuntimeRequirementId::new("windows-vc").is_err());
    assert!(RuntimeRequirementId::new("windows.vc_").is_err());
    assert!(RuntimeRequirementId::new("7windows.vc").is_err());
}

#[test]
fn installed_package_ids_accept_provider_owned_identities() {
    let guid = InstalledPackageId::new("{F3017226-FE2A-4295-8A7C-971BF3207148}").unwrap();
    assert_eq!(guid.as_str(), "{F3017226-FE2A-4295-8A7C-971BF3207148}");
    assert!(InstalledPackageId::new("org.example.product").is_ok());
    assert!(InstalledPackageId::new("  ").is_err());
    assert!(InstalledPackageId::new("{ spaced }").is_err());
    assert!(InstalledPackageId::new("sub\\key\\product").is_err());
    assert!(InstalledPackageId::new("c:product").is_err());
    assert!(InstalledPackageId::new("product\ncode").is_err());
}

/// A requirement must be expressible without a registry hive, key, or product
/// code, or an installer authored on Windows cannot be evaluated elsewhere.
#[test]
fn requirements_expose_only_portable_semantics() {
    let runtime = PrerequisiteRequirement::Runtime(Runtime {
        id: RuntimeRequirementId::new("windows.vc.v14").unwrap(),
        version: Some(VersionReq::parse(">=14.0.0").unwrap()),
    });
    assert_eq!(runtime.kind_name(), "runtime");
    let package = PrerequisiteRequirement::InstalledPackage(InstalledPackage {
        id: InstalledPackageId::new("{F3017226-FE2A-4295-8A7C-971BF3207148}").unwrap(),
        version: None,
    });
    assert_eq!(package.kind_name(), "installed_package");
    let file = PrerequisiteRequirement::FileVersion(zup_core::FileVersion {
        path: Template::parse("C:/Program Files/Acme/host.exe").unwrap(),
        version: Some(VersionReq::parse(">=1.0").unwrap()),
    });
    assert_eq!(file.kind_name(), "file_version");
    for requirement in [&runtime, &package, &file] {
        let json = serde_json::to_value(requirement).unwrap();
        for legacy in ["product_code", "hive", "key"] {
            assert!(
                json.get(legacy).is_none(),
                "{legacy} leaked back into {json}"
            );
        }
    }
}
