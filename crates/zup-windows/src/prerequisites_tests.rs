use super::*;
use std::sync::OnceLock;
use windows::Win32::System::ApplicationInstallationAndServicing::*;
use windows::core::PCWSTR;

fn without_admin_access(action: impl FnOnce()) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut raw = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut raw).unwrap();
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut admin = [0u32; 17];
    let mut size = std::mem::size_of_val(&admin) as u32;
    unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            None,
            Some(PSID(admin.as_mut_ptr().cast())),
            &mut size,
        )
        .unwrap();
    }
    let mut raw = HANDLE::default();
    unsafe {
        CreateRestrictedToken(
            HANDLE(token.as_raw_handle()),
            DISABLE_MAX_PRIVILEGE,
            Some(&[SID_AND_ATTRIBUTES {
                Sid: PSID(admin.as_mut_ptr().cast()),
                Attributes: 0,
            }]),
            None,
            None,
            &mut raw,
        )
        .unwrap();
    }
    let restricted = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    unsafe {
        ImpersonateLoggedOnUser(HANDLE(restricted.as_raw_handle())).unwrap();
    }
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            unsafe {
                RevertToSelf().unwrap();
            }
        }
    }
    let _revert = Revert;
    action();
}

fn executable_fixture() -> &'static Path {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("fixture.rs");
            let output = root.path().join("fixture.exe");
            std::fs::write(
                &source,
                include_str!("../tests/fixture/prerequisite_installer.rs"),
            )
            .unwrap();
            assert!(
                Command::new("rustc")
                    .arg(&source)
                    .arg("-o")
                    .arg(&output)
                    .status()
                    .unwrap()
                    .success()
            );
            (root, output)
        })
        .1
}

struct Msi(MSIHANDLE);
impl Drop for Msi {
    fn drop(&mut self) {
        unsafe {
            MsiCloseHandle(self.0);
        }
    }
}

fn msi_fixture(path: &Path, marker: &Path) {
    make_msi_fixture(path, marker, false);
}
fn make_msi_fixture(path: &Path, marker: &Path, register: bool) -> String {
    let source_dir = path.parent().unwrap().display().to_string() + "\\";
    let product = format!("{{{}}}", uuid::Uuid::new_v4()).to_uppercase();
    let mut database = MSIHANDLE::default();
    let path = wide(path.to_str().unwrap());
    assert_eq!(
        unsafe {
            MsiOpenDatabaseW(
                PCWSTR(path.as_ptr()),
                PCWSTR(3usize as *const u16),
                &mut database,
            )
        },
        0
    );
    let database = Msi(database);
    let sql = |query: &str| {
        let query = wide(query);
        let mut view = MSIHANDLE::default();
        assert_eq!(
            unsafe { MsiDatabaseOpenViewW(database.0, PCWSTR(query.as_ptr()), &mut view) },
            0
        );
        let view = Msi(view);
        assert_eq!(unsafe { MsiViewExecute(view.0, MSIHANDLE::default()) }, 0);
    };
    sql(
        "CREATE TABLE `Property` (`Property` CHAR(72) NOT NULL, `Value` CHAR(0) LOCALIZABLE PRIMARY KEY `Property`)",
    );
    for (key, value) in [
        ("ProductCode", product.clone()),
        ("SourceDir", source_dir),
        ("ProductName", "Zup inventory fixture".into()),
        ("ProductVersion", "1.0.0".into()),
        ("ProductLanguage", "1033".into()),
        ("Manufacturer", "Zup test".into()),
        (
            "Cmd",
            system_directory()
                .unwrap()
                .join("cmd.exe")
                .display()
                .to_string(),
        ),
    ] {
        sql(&format!(
            "INSERT INTO `Property` (`Property`, `Value`) VALUES ('{key}', '{}')",
            value.replace('\'', "''")
        ));
    }
    sql(
        "CREATE TABLE `CustomAction` (`Action` CHAR(72) NOT NULL, `Type` SHORT NOT NULL, `Source` CHAR(72), `Target` CHAR(255) LOCALIZABLE PRIMARY KEY `Action`)",
    );
    let target = format!("/d /c echo authenticated>\"{}\"", marker.display()).replace('\'', "''");
    sql(&format!(
        "INSERT INTO `CustomAction` (`Action`, `Type`, `Source`, `Target`) VALUES ('Marker', 50, 'Cmd', '{target}')"
    ));
    sql(
        "CREATE TABLE `InstallExecuteSequence` (`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT PRIMARY KEY `Action`)",
    );
    sql("INSERT INTO `InstallExecuteSequence` (`Action`, `Sequence`) VALUES ('Marker', 1)");
    if register {
        sql(
            "CREATE TABLE `AdvtExecuteSequence` (`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT PRIMARY KEY `Action`)",
        );
        for (action, sequence) in [
            ("CostInitialize", 800),
            ("CostFinalize", 1000),
            ("InstallValidate", 1400),
            ("InstallInitialize", 1500),
            ("PublishFeatures", 6300),
            ("PublishProduct", 6400),
            ("InstallFinalize", 6600),
        ] {
            sql(&format!(
                "INSERT INTO `AdvtExecuteSequence` (`Action`, `Sequence`) VALUES ('{action}', {sequence})"
            ));
        }
        sql(
            "CREATE TABLE `Media` (`DiskId` SHORT NOT NULL, `LastSequence` LONG NOT NULL, `DiskPrompt` CHAR(64) LOCALIZABLE, `Cabinet` CHAR(255), `VolumeLabel` CHAR(32), `Source` CHAR(72) PRIMARY KEY `DiskId`)",
        );
        sql("INSERT INTO `Media` (`DiskId`, `LastSequence`) VALUES (1, 0)");
        sql(
            "CREATE TABLE `Directory` (`Directory` CHAR(72) NOT NULL, `Directory_Parent` CHAR(72), `DefaultDir` CHAR(255) NOT NULL PRIMARY KEY `Directory`)",
        );
        sql(
            "INSERT INTO `Directory` (`Directory`, `DefaultDir`) VALUES ('TARGETDIR', 'SourceDir')",
        );
        sql(
            "CREATE TABLE `Feature` (`Feature` CHAR(38) NOT NULL, `Feature_Parent` CHAR(38), `Title` CHAR(64) LOCALIZABLE, `Description` CHAR(255) LOCALIZABLE, `Display` SHORT, `Level` SHORT NOT NULL, `Directory_` CHAR(72), `Attributes` SHORT NOT NULL PRIMARY KEY `Feature`)",
        );
        sql("INSERT INTO `Feature` (`Feature`, `Level`, `Attributes`) VALUES ('Inventory', 1, 0)");
        sql(
            "CREATE TABLE `FeatureComponents` (`Feature_` CHAR(38) NOT NULL, `Component_` CHAR(72) NOT NULL PRIMARY KEY `Feature_`, `Component_`)",
        );
        sql(
            "CREATE TABLE `File` (`File` CHAR(72) NOT NULL, `Component_` CHAR(72) NOT NULL, `FileName` CHAR(255) NOT NULL, `FileSize` LONG NOT NULL, `Version` CHAR(72), `Language` CHAR(20), `Attributes` SHORT, `Sequence` LONG NOT NULL PRIMARY KEY `File`)",
        );
        sql(
            "CREATE TABLE `Component` (`Component` CHAR(72) NOT NULL, `ComponentId` CHAR(38), `Directory_` CHAR(72) NOT NULL, `Attributes` SHORT NOT NULL, `Condition` CHAR(255), `KeyPath` CHAR(72) PRIMARY KEY `Component`)",
        );
        sql(
            "CREATE TABLE `Registry` (`Registry` CHAR(72) NOT NULL, `Root` SHORT NOT NULL, `Key` CHAR(255) NOT NULL, `Name` CHAR(255), `Value` CHAR(0) LOCALIZABLE, `Component_` CHAR(72) NOT NULL PRIMARY KEY `Registry`)",
        );
        sql(&format!(
            "INSERT INTO `Component` (`Component`, `ComponentId`, `Directory_`, `Attributes`, `KeyPath`) VALUES ('Inventory', '{}', 'TARGETDIR', 4, 'Inventory')",
            format!("{{{}}}", uuid::Uuid::new_v4()).to_uppercase()
        ));
        sql(
            "INSERT INTO `FeatureComponents` (`Feature_`, `Component_`) VALUES ('Inventory', 'Inventory')",
        );
        sql(&format!(
            "INSERT INTO `Registry` (`Registry`, `Root`, `Key`, `Name`, `Value`, `Component_`) VALUES ('Inventory', 1, 'Software\\Zup\\Tests\\{product}', 'Fixture', '1', 'Inventory')"
        ));
        for (action, sequence) in [
            ("CostInitialize", 800),
            ("FileCost", 900),
            ("CostFinalize", 1000),
            ("InstallValidate", 1400),
            ("InstallInitialize", 1500),
            ("ProcessComponents", 1600),
            ("UnpublishFeatures", 1800),
            ("RemoveRegistryValues", 2600),
            ("WriteRegistryValues", 5000),
            ("RegisterProduct", 6100),
            ("PublishFeatures", 6300),
            ("PublishProduct", 6400),
            ("InstallFinalize", 6600),
        ] {
            sql(&format!(
                "INSERT INTO `InstallExecuteSequence` (`Action`, `Sequence`) VALUES ('{action}', {sequence})"
            ));
        }
    }
    let mut summary = MSIHANDLE::default();
    assert_eq!(
        unsafe { MsiGetSummaryInformationW(database.0, PCWSTR::null(), 4, &mut summary) },
        0
    );
    let summary = Msi(summary);
    for (property, value) in [
        (7, "Intel;1033".to_owned()),
        (9, format!("{{{}}}", uuid::Uuid::new_v4()).to_uppercase()),
    ] {
        let value = wide(&value);
        assert_eq!(
            unsafe {
                MsiSummaryInfoSetPropertyW(
                    summary.0,
                    property,
                    30,
                    0,
                    std::ptr::null_mut(),
                    PCWSTR(value.as_ptr()),
                )
            },
            0
        );
    }
    assert_eq!(
        unsafe {
            MsiSummaryInfoSetPropertyW(summary.0, 14, 3, 500, std::ptr::null_mut(), PCWSTR::null())
        },
        0
    );
    assert_eq!(
        unsafe {
            MsiSummaryInfoSetPropertyW(summary.0, 15, 3, 8, std::ptr::null_mut(), PCWSTR::null())
        },
        0
    );
    assert_eq!(unsafe { MsiSummaryInfoPersist(summary.0) }, 0);
    assert_eq!(unsafe { MsiDatabaseCommit(database.0) }, 0);
    product
}

fn request(source: PathBuf) -> ProviderRequest {
    let (size, sha256) = zup_core::hash_reader(std::fs::File::open(&source).unwrap()).unwrap();
    ProviderRequest {
        prerequisite_id: zup_core::PrerequisiteId::new("fixture").unwrap(),
        requirement: PrerequisiteRequirement::InstalledPackage(InstalledPackage {
            id: zup_core::InstalledPackageId::new("{00000000-0000-0000-0000-000000000000}")
                .unwrap(),
            version: None,
        }),
        package: zup_core::PrerequisitePackage::Remote {
            url: "https://example.test/fixture".into(),
            sha256,
            size: Some(size),
            filename: source.file_name().unwrap().to_str().unwrap().into(),
        },
        executable: source,
        arguments: vec![],
        success_exit_codes: vec![0],
        reboot_exit_codes: vec![3010],
    }
}

fn replacement_fixture(path: &Path) {
    let mut bytes = std::fs::read(executable_fixture()).unwrap();
    let offset = bytes
        .windows(b"authenticated".len())
        .position(|part| part == b"authenticated")
        .unwrap();
    bytes[offset..offset + 13].copy_from_slice(b"unverified!!!");
    std::fs::write(path, bytes).unwrap();
}

fn substitution_case(msi: bool, exit_code: i32) {
    let root = tempfile::tempdir().unwrap();
    let source = root
        .path()
        .join(if msi { "fixture.msi" } else { "fixture.exe" });
    let marker = root.path().join("marker");
    let replacement = root.path().join(if msi {
        "replacement.msi"
    } else {
        "replacement.exe"
    });
    let foreign_marker = root.path().join("unverified-marker");
    if msi {
        msi_fixture(&source, &marker);
        msi_fixture(&replacement, &foreign_marker);
    } else {
        std::fs::copy(executable_fixture(), &source).unwrap();
        replacement_fixture(&replacement);
    }
    let mut request = request(source.clone());
    let log = root.path().join("msi.log");
    if msi {
        request.arguments = vec!["/l*v".into(), log.display().to_string()];
    }
    if !msi {
        request.arguments = vec![
            "--marker".into(),
            marker.display().to_string(),
            "--exit".into(),
            exit_code.to_string(),
        ];
    }
    let outcome = execute_held(&request, |artifact| {
        let staged = artifact.source.path();
        if !crate::transport::is_process_elevated().unwrap() {
            assert!(std::fs::copy(&replacement, staged).is_err());
            assert!(std::fs::rename(&replacement, staged).is_err());
            assert!(std::fs::write(staged, b"substituted").is_err());
            assert!(std::fs::rename(staged, root.path().join("stolen")).is_err());
            assert!(std::fs::remove_file(staged).is_err());
            assert!(std::fs::rename(root.path(), root.path().with_extension("moved")).is_err());
        } else {
            assert_ne!(staged, source);
            std::fs::copy(&replacement, &source).unwrap();
            without_admin_access(|| {
                assert!(std::fs::copy(&replacement, staged).is_err());
                assert!(std::fs::rename(&replacement, staged).is_err());
                assert!(std::fs::write(staged, b"substituted").is_err());
                assert!(std::fs::rename(staged, root.path().join("stolen")).is_err());
                assert!(std::fs::remove_file(staged).is_err());
                assert!(
                    std::fs::rename(staged.parent().unwrap(), root.path().join("redirected"))
                        .is_err()
                );
                assert!(
                    std::fs::hard_link(&source, staged.parent().unwrap().join("alternate"))
                        .is_err()
                );
                assert!(std::os::windows::fs::symlink_file(&source, staged).is_err());
            });
        }
    })
    .unwrap_or_else(|error| {
        let bytes = std::fs::read(&log).unwrap_or_default();
        let text = String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
        panic!("{error}: {text}");
    });
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().trim(),
        "authenticated"
    );
    assert!(!foreign_marker.exists());
    assert_eq!(
        outcome,
        if exit_code == 3010 {
            ProviderOutcome::RebootRequired { exit_code }
        } else {
            ProviderOutcome::Succeeded
        }
    );
}

#[test]
fn native_execution_unheld_path_reproduces_verify_use_substitution() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("fixture.exe");
    let marker = root.path().join("marker");
    std::fs::copy(executable_fixture(), &source).unwrap();
    let expected = request(source.clone());
    let file = std::fs::File::open(&source).unwrap();
    assert_eq!(
        zup_core::hash_reader(&file).unwrap().1,
        expected.package.digest().unwrap()
    );
    drop(file);
    replacement_fixture(&source);
    assert!(
        Command::new(source)
            .arg("--marker")
            .arg(&marker)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"unverified!!!");
}

#[test]
fn native_execution_exe_survives_substitution_and_reports_reboot() {
    substitution_case(false, 0);
    substitution_case(false, 3010);
}

#[test]
fn native_execution_msi_reads_held_source_and_runs_action() {
    substitution_case(true, 0);
}

#[test]
#[ignore = "requires an elevated Windows token"]
fn native_execution_elevated_copies_untrusted_acquisition() {
    assert!(crate::transport::is_process_elevated().unwrap());
    substitution_case(false, 0);
    substitution_case(true, 0);
}

#[test]
fn native_execution_refuses_missing_tampered_and_redirected_sources() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("fixture.exe");
    std::fs::copy(executable_fixture(), &source).unwrap();
    let mut expected = request(source.clone());
    std::fs::write(&source, b"different bytes").unwrap();
    assert!(matches!(
        WindowsPrerequisiteProvider.execute(&expected),
        Err(BootstrapError::ProviderPreflight(_))
    ));
    expected = request(source.clone());
    assert!(matches!(
        WindowsPrerequisiteProvider.execute(&expected),
        Err(BootstrapError::ProviderPreflight(_))
    ));
    expected.package = zup_core::PrerequisitePackage::DetectOnly;
    assert!(matches!(
        WindowsPrerequisiteProvider.execute(&expected),
        Err(BootstrapError::ProviderPreflight(_))
    ));
    std::fs::remove_file(&source).unwrap();
    assert!(matches!(
        WindowsPrerequisiteProvider.execute(&request_for_missing(source.clone())),
        Err(BootstrapError::ProviderPreflight(_))
    ));

    let real = root.path().join("real");
    let redirected = root.path().join("redirected");
    std::fs::create_dir(&real).unwrap();
    std::fs::copy(executable_fixture(), real.join("fixture.exe")).unwrap();
    let status = Command::new(system_directory().unwrap().join("cmd.exe"))
        .args(["/d", "/c", "mklink", "/j"])
        .arg(&redirected)
        .arg(&real)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let expected = request(redirected.join("fixture.exe"));
    assert!(matches!(
        WindowsPrerequisiteProvider.execute(&expected),
        Err(BootstrapError::ProviderPreflight(_))
    ));
    std::fs::remove_dir(redirected).unwrap();
}

fn request_for_missing(path: PathBuf) -> ProviderRequest {
    let mut expected = request(executable_fixture().to_owned());
    expected.executable = path;
    expected
}

fn wait_for(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture did not reach synchronization barrier"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
#[ignore = "subprocess entry point"]
fn native_execution_interrupted_worker_child() {
    let source = PathBuf::from(std::env::var_os("ZUP_F06_SOURCE").unwrap());
    let root = source.parent().unwrap();
    let mut expected = request(source.clone());
    expected.arguments = [
        "--marker",
        "marker",
        "--ready",
        "ready",
        "--release",
        "release",
    ]
    .as_chunks::<2>()
    .0
    .iter()
    .flat_map(|pair| [pair[0].to_owned(), root.join(pair[1]).display().to_string()])
    .collect();
    execute_held(&expected, |artifact| {
        std::fs::write(
            root.join("custody"),
            artifact.source.path().to_str().unwrap(),
        )
        .unwrap();
    })
    .unwrap();
}

fn interruption_case() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("fixture.exe");
    std::fs::copy(executable_fixture(), &source).unwrap();
    let mut worker = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "prerequisites::native_tests::native_execution_interrupted_worker_child",
        ])
        .env("ZUP_F06_SOURCE", &source)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&root.path().join("ready"));
    worker.kill().unwrap();
    worker.wait().unwrap();
    let custody = PathBuf::from(std::fs::read_to_string(root.path().join("custody")).unwrap());
    if crate::transport::is_process_elevated().unwrap() {
        without_admin_access(|| {
            assert!(std::fs::write(&custody, b"after worker crash").is_err());
        });
    }
    std::fs::write(root.path().join("release"), b"continue").unwrap();
    wait_for(&root.path().join("marker"));
    assert_eq!(
        std::fs::read(root.path().join("marker")).unwrap(),
        b"authenticated"
    );
}

#[test]
fn native_execution_child_completes_after_worker_interruption() {
    interruption_case();
}

#[test]
#[ignore = "requires an elevated Windows token"]
fn native_execution_elevated_custody_survives_worker_interruption() {
    assert!(crate::transport::is_process_elevated().unwrap());
    interruption_case();
}

#[test]
fn native_msi_version_length_contract() {
    native_msi_inventory_case(false);
}
#[test]
#[ignore = "requires an elevated Windows token"]
fn native_msi_machine_inventory() {
    assert!(crate::transport::is_process_elevated().unwrap());
    native_msi_inventory_case(true);
}
fn native_msi_inventory_case(machine: bool) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("inventory.msi");
    let product = make_msi_fixture(&path, &root.path().join("marker"), true);
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let status = Command::new(system_directory().unwrap().join("msiexec.exe"))
                .args(["/x", &self.0, "/qn", "/norestart"])
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(0));
        }
    }
    let log = root.path().join("msi.log");
    assert_eq!(
        Command::new(system_directory().unwrap().join("msiexec.exe"))
            .arg("/i")
            .arg(&path)
            .args([
                "/qn",
                "/norestart",
                "/l*v",
                log.to_str().unwrap(),
                if machine { "ALLUSERS=1" } else { "ALLUSERS=2" },
                if machine {
                    "MSIINSTALLPERUSER="
                } else {
                    "MSIINSTALLPERUSER=1"
                }
            ])
            .status()
            .unwrap()
            .code(),
        Some(0)
    );
    let cleanup = Cleanup(product.clone());
    let context = if machine {
        MSIINSTALLCONTEXT_MACHINE
    } else {
        MSIINSTALLCONTEXT_USERUNMANAGED
    };
    assert_eq!(
        product_property(&product, context, "ProductName")
            .unwrap()
            .unwrap(),
        "Zup inventory fixture"
    );
    let product_w = wide(&product);
    let property = wide("VersionString");
    let mut length = 0;
    assert_eq!(
        unsafe {
            MsiGetProductInfoW(
                PCWSTR(product_w.as_ptr()),
                PCWSTR(property.as_ptr()),
                None,
                Some(&mut length),
            )
        },
        0
    );
    assert_eq!(length, 5);
    let mut old = vec![0u16; length as usize];
    assert_eq!(
        unsafe {
            MsiGetProductInfoW(
                PCWSTR(product_w.as_ptr()),
                PCWSTR(property.as_ptr()),
                Some(windows::core::PWSTR(old.as_mut_ptr())),
                Some(&mut length),
            )
        },
        234
    );
    assert_eq!(
        product_property(&product, context, "VersionString")
            .unwrap()
            .unwrap(),
        "1.0.0"
    );
    let package = InstalledPackage {
        id: zup_core::InstalledPackageId::new(&product).unwrap(),
        version: Some(VersionReq::parse("=1.0.0").unwrap()),
    };
    assert!(matches!(
        satisfy_installed_package(
            &package,
            PrerequisiteArchitecture::X86,
            if machine {
                zup_core::SelectedScope::Machine
            } else {
                zup_core::SelectedScope::User
            }
        )
        .unwrap(),
        DetectionResult::Satisfied { .. }
    ));
    if !machine {
        assert_eq!(
            satisfy_installed_package(
                &package,
                PrerequisiteArchitecture::Any,
                zup_core::SelectedScope::Machine
            )
            .unwrap(),
            DetectionResult::Missing
        );
    }
    assert_native_bootstrap(
        PrerequisiteRequirement::InstalledPackage(package.clone()),
        PrerequisiteArchitecture::X86,
        if machine {
            zup_core::SelectedScope::Machine
        } else {
            zup_core::SelectedScope::User
        },
    );
    assert!(
        satisfy_installed_package(
            &package,
            PrerequisiteArchitecture::X64,
            zup_core::SelectedScope::User
        )
        .is_err()
    );
    let wrong_version = InstalledPackage {
        version: Some(VersionReq::parse(">=2").unwrap()),
        ..package.clone()
    };
    assert!(matches!(
        satisfy_installed_package(
            &wrong_version,
            PrerequisiteArchitecture::Any,
            zup_core::SelectedScope::User
        )
        .unwrap(),
        DetectionResult::Incompatible { .. }
    ));
    drop(cleanup);
    assert_eq!(
        satisfy_installed_package(
            &package,
            PrerequisiteArchitecture::Any,
            zup_core::SelectedScope::User
        )
        .unwrap(),
        DetectionResult::Missing
    );
    if !machine {
        assert_eq!(
            Command::new(system_directory().unwrap().join("msiexec.exe"))
                .arg("/ju")
                .arg(&path)
                .arg("/qn")
                .status()
                .unwrap()
                .code(),
            Some(0)
        );
        let advertised_cleanup = Cleanup(product.clone());
        assert_eq!(
            product_property(&product, MSIINSTALLCONTEXT_USERUNMANAGED, "State")
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            satisfy_installed_package(
                &package,
                PrerequisiteArchitecture::Any,
                zup_core::SelectedScope::User
            )
            .unwrap(),
            DetectionResult::Missing
        );
        drop(advertised_cleanup);
    }
}

#[test]
fn inventory_selects_an_installed_matching_version() {
    let versions = ["6.0.36", "8.0.12", "9.0.1", "10.0.0", "11.0.0-preview.1"]
        .map(|v| Version::parse(v).unwrap())
        .to_vec();
    let requirement = VersionReq::parse("8.*").unwrap();
    assert!(
        matches!(select_version(versions.clone(), Some(&requirement), "fixture".into()).unwrap(), DetectionResult::Satisfied { version: Some(v), .. } if v == Version::new(8,0,12))
    );
    assert!(matches!(
        select_version(
            versions,
            Some(&VersionReq::parse("7.*").unwrap()),
            "fixture".into()
        )
        .unwrap(),
        DetectionResult::Incompatible { .. }
    ));
    assert_eq!(
        select_version(vec![], None, "fixture".into()).unwrap(),
        DetectionResult::Missing
    );
    for invalid in [
        "1..2",
        "1.2.3.4.5",
        "1.2.invalid",
        "1.2.3.garbage",
        "-1.2.3",
    ] {
        assert!(parse_runtime_version(invalid).is_none(), "{invalid}");
    }
    for valid in [
        "v14.42.34433.0",
        "131.0.2903.112",
        "1.2.3",
        "8.0.0-preview.1",
    ] {
        assert!(parse_runtime_version(valid).is_some(), "{valid}");
    }
    assert_eq!(
        parse_runtime_version("131.0.2903.112")
            .unwrap()
            .build
            .as_str(),
        "112"
    );
}

#[test]
fn msi_reader_retries_growth_and_preserves_utf16() {
    let value = wide("Ã¦â€”Â¥Ã¦Å“Â¬Ã¨ÂªÅ¾ Ã°Å¸Â¦â‚¬");
    let mut call = 0;
    let actual = read_msi_string(|buffer, length| {
        call += 1;
        unsafe {
            if call == 1 {
                *length = 1;
                return 0;
            }
            if call == 2 {
                *length = (value.len() - 1) as u32;
                return 234;
            }
            assert!(*length as usize >= value.len());
            std::ptr::copy_nonoverlapping(value.as_ptr(), buffer.unwrap().0, value.len());
            *length = (value.len() - 1) as u32;
        }
        0
    })
    .unwrap();
    assert_eq!(actual.as_deref(), Some("Ã¦â€”Â¥Ã¦Å“Â¬Ã¨ÂªÅ¾ Ã°Å¸Â¦â‚¬"));
    assert!(read_msi_string(|_, _| 5).is_err());
    assert_eq!(read_msi_string(|_, _| 1605).unwrap(), None);
    assert!(
        read_msi_string(|_, length| {
            unsafe {
                *length = 40000;
            }
            234
        })
        .is_err()
    );
}

#[test]
fn native_dotnet_inventory_agrees_with_installed_host() {
    let Some(key) = open_machine_key(
        &format!(
            "{DOTNET_INSTALLED_VERSIONS_KEY}\\{}",
            architecture_name(PrerequisiteArchitecture::Current)
        ),
        PrerequisiteArchitecture::X86,
    )
    .unwrap() else {
        return;
    };
    let location = key.get_string("InstallLocation").unwrap();
    let output = Command::new(Path::new(&location).join("dotnet.exe"))
        .arg("--list-runtimes")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for family in ["Microsoft.NETCore.App", "Microsoft.WindowsDesktop.App"] {
        let versions: Vec<_> = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == family).then(|| Version::parse(fields.next().unwrap()).unwrap())
            })
            .collect();
        if !versions.is_empty() {
            let id = if family == "Microsoft.NETCore.App" {
                runtime_requirements::DOTNET_RUNTIME
            } else {
                runtime_requirements::DOTNET_DESKTOP
            };
            assert_native_bootstrap(
                PrerequisiteRequirement::Runtime(Runtime {
                    id: zup_core::RuntimeRequirementId::new(id).unwrap(),
                    version: None,
                }),
                PrerequisiteArchitecture::Current,
                zup_core::SelectedScope::User,
            );
        }
        for version in &versions {
            let requirement = VersionReq::parse(&format!("={version}")).unwrap();
            assert!(matches!(
                detect_dotnet(
                    PrerequisiteArchitecture::Current,
                    family,
                    Some(&requirement),
                    family.into()
                )
                .unwrap(),
                DetectionResult::Satisfied { .. }
            ));
        }
        if versions.is_empty() {
            assert_eq!(
                detect_dotnet(
                    PrerequisiteArchitecture::Current,
                    family,
                    None,
                    family.into()
                )
                .unwrap(),
                DetectionResult::Missing
            );
        }
    }
}

#[test]
fn native_runtime_inventory_and_file_versions_are_readable() {
    for id in [
        runtime_requirements::VISUAL_CPP_V14,
        runtime_requirements::WEBVIEW2_EVERGREEN,
    ] {
        let operation = BootstrapOperation {
            id: zup_core::PrerequisiteId::new("inventory").unwrap(),
            name: "Inventory".into(),
            target: PrerequisiteArchitecture::Any,
            requirement: PrerequisiteRequirement::Runtime(Runtime {
                id: zup_core::RuntimeRequirementId::new(id).unwrap(),
                version: None,
            }),
            package: zup_core::PrerequisitePackage::DetectOnly,
            installer: Default::default(),
        };
        WindowsPrerequisiteDetector::new(zup_core::SelectedScope::User)
            .satisfy(&operation)
            .unwrap();
    }
    let version = read_file_version(&system_directory().unwrap().join("kernel32.dll"))
        .unwrap()
        .unwrap();
    assert!(version.major > 0);
    assert!(!version.build.is_empty());
}

fn assert_native_bootstrap(
    requirement: PrerequisiteRequirement,
    target: PrerequisiteArchitecture,
    scope: zup_core::SelectedScope,
) {
    use zup_bootstrap::{
        BootstrapKey, BootstrapOperationState, BootstrapOutcome, BootstrapPlan, BootstrapState,
    };
    let operation = BootstrapOperation {
        id: zup_core::PrerequisiteId::new("native").unwrap(),
        name: "Native fixture".into(),
        target,
        requirement,
        package: zup_core::PrerequisitePackage::DetectOnly,
        installer: Default::default(),
    };
    let plan = BootstrapPlan::new(
        BootstrapKey {
            app_id: zup_core::AppId::new("com.zup.test.f16").unwrap(),
            app_version: Version::new(1, 0, 0),
            scope,
            target: zup_core::Target::parse("windows-x64").unwrap(),
        },
        vec![operation],
    )
    .unwrap();
    let detector = WindowsPrerequisiteDetector::new(scope);
    let mut state = BootstrapState::new(&plan);
    assert!(
        zup_bootstrap::assess(&plan, &detector, &mut state)
            .unwrap()
            .is_empty()
    );
    state
        .mark(&plan.operations[0].id, BootstrapOperationState::Running)
        .unwrap();
    assert_eq!(
        zup_bootstrap::recover(&plan, &detector, &mut state).unwrap(),
        BootstrapOutcome::Ready
    );
    assert_eq!(
        zup_bootstrap::execute_operation(
            &plan,
            &plan.operations[0],
            &detector,
            &WindowsPrerequisiteProvider,
            PathBuf::new(),
            &mut state
        )
        .unwrap(),
        BootstrapOutcome::Ready
    );
}

#[test]
fn invalid_product_identity_never_becomes_absence() {
    for identity in [
        "not-a-product",
        "{000000000000000000000000000000000000}",
        "{日本語00000000000000000000000000000}",
    ] {
        let package = InstalledPackage {
            id: zup_core::InstalledPackageId::new(identity).unwrap(),
            version: None,
        };
        assert!(
            satisfy_installed_package(
                &package,
                PrerequisiteArchitecture::Any,
                zup_core::SelectedScope::User
            )
            .is_err()
        );
    }
}
