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
        ("ProductCode", format!("{{{}}}", uuid::Uuid::new_v4())),
        ("ProductName", "Zup custody fixture".into()),
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
    // This package only runs an immediate, harmless fixture action. It registers no product.
    sql("INSERT INTO `InstallExecuteSequence` (`Action`, `Sequence`) VALUES ('Marker', 1)");
    let mut summary = MSIHANDLE::default();
    assert_eq!(
        unsafe { MsiGetSummaryInformationW(database.0, PCWSTR::null(), 3, &mut summary) },
        0
    );
    let summary = Msi(summary);
    for (property, value) in [
        (7, "Intel;1033".to_owned()),
        (9, format!("{{{}}}", uuid::Uuid::new_v4())),
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
            MsiSummaryInfoSetPropertyW(summary.0, 14, 3, 200, std::ptr::null_mut(), PCWSTR::null())
        },
        0
    );
    assert_eq!(unsafe { MsiSummaryInfoPersist(summary.0) }, 0);
    assert_eq!(unsafe { MsiDatabaseCommit(database.0) }, 0);
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
