use semver::VersionReq;
use zup_core::{
    App, AppId, Install, InstallDirectory, InstallScope, InstalledPackage, InstalledPackageId,
    Installer, NonEmptyString, PrerequisiteArchitecture, PrerequisiteId, PrerequisitePackage,
    PrerequisiteRequirement, RelativePath, Runtime, RuntimeRequirementId, Sha256Digest,
    TargetTriple, Template,
};

fn digest(byte: u8) -> Sha256Digest {
    Sha256Digest::from_bytes([byte; 32])
}

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
        assert!(json.get("kind").is_some());
        assert!(json.get("product_code").is_none());
        assert!(json.get("hive").is_none());
        assert!(json.get("key").is_none());
        let restored: PrerequisiteRequirement = serde_json::from_value(json).unwrap();
        assert_eq!(&restored, requirement);
    }
}

#[test]
fn prerequisite_package_roundtrips_with_exact_digest() {
    let prerequisite = zup_core::Prerequisite {
        id: PrerequisiteId::new("runtime").unwrap(),
        name: NonEmptyString::new("Runtime").unwrap(),
        description: None,
        component: None,
        when: None,
        target: PrerequisiteArchitecture::Current,
        requirement: PrerequisiteRequirement::Runtime(Runtime {
            id: RuntimeRequirementId::new("windows.vc.v14").unwrap(),
            version: Some(VersionReq::parse(">=14.0.0").unwrap()),
        }),
        package: PrerequisitePackage::Remote {
            url: "https://example.test/runtime.exe".into(),
            sha256: digest(7),
            size: Some(10),
            filename: "runtime.exe".into(),
        },
        installer: Default::default(),
    };
    let json = serde_json::to_string(&prerequisite).unwrap();
    let restored: zup_core::Prerequisite = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, prerequisite);
}

#[test]
fn installer_carries_no_package_format_selection() {
    let json = serde_json::to_value(zup_core::PrerequisiteInstaller::default()).unwrap();
    assert!(json.get("kind").is_none());
    assert_eq!(json["arguments"], serde_json::json!([]));
    assert_eq!(json["success_exit_codes"], serde_json::json!([0]));
    assert_eq!(json["reboot_exit_codes"], serde_json::json!([1641, 3010]));
    assert_eq!(json["privilege"], serde_json::json!("system"));
}

#[test]
fn installer_without_prerequisites_serializes_with_an_empty_collection() {
    let installer = Installer {
        app: App {
            id: AppId::new("com.example.app").unwrap(),
            name: NonEmptyString::new("App").unwrap(),
            version: "1.0.0".parse().unwrap(),
            publisher: None,
            main: None,
            description: None,
        },
        frontend: Default::default(),
        target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
        ui: None,
        updates: None,
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse("${location.user_data}/App").unwrap()),
                machine: None,
            },
            allow_directory_override: false,
        },
        prerequisites: Vec::new(),
        components: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    };
    let json = serde_json::to_value(&installer).unwrap();
    assert_eq!(json["prerequisites"], serde_json::json!([]));
}

#[test]
fn relative_embedded_package_path_is_portable() {
    let path = RelativePath::new("prerequisites/runtime.exe").unwrap();
    assert_eq!(path.as_str(), "prerequisites/runtime.exe");
}
