use semver::VersionReq;
use zup_core::{
    App, AppId, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
    PrerequisiteArchitecture, PrerequisiteDetector, PrerequisiteId, PrerequisitePackage,
    RelativePath, Sha256Digest, Template,
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
fn prerequisite_package_roundtrips_with_exact_digest() {
    let prerequisite = zup_core::Prerequisite {
        id: PrerequisiteId::new("runtime").unwrap(),
        name: NonEmptyString::new("Runtime").unwrap(),
        description: None,
        component: None,
        when: None,
        target: PrerequisiteArchitecture::Current,
        detector: PrerequisiteDetector::VisualCppV14 {
            version: Some(VersionReq::parse(">=14.0.0").unwrap()),
        },
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
        ui: None,
        updates: None,
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse("${known.local_app_data}/App").unwrap()),
                machine: None,
            },
            allow_directory_override: false,
        },
        prerequisites: Vec::new(),
        components: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        shortcuts: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_types: Vec::new(),
    };
    let json = serde_json::to_value(&installer).unwrap();
    assert_eq!(json["prerequisites"], serde_json::json!([]));
}

#[test]
fn relative_embedded_package_path_is_portable() {
    let path = RelativePath::new("prerequisites/runtime.exe").unwrap();
    assert_eq!(path.as_str(), "prerequisites/runtime.exe");
}
