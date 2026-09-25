use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use semver::{Version, VersionReq};
use windows_registry::{Key, LOCAL_MACHINE, Type};
use zup_bootstrap::{
    BootstrapError, BootstrapOperation, BuiltinProvider, DetectionResult, PrerequisiteDetector,
    PrerequisiteProvider, ProviderOutcome, ProviderRequest,
};
use zup_core::{
    PrerequisiteArchitecture, PrerequisiteDetector as PrerequisiteDetectorSpec, RegistryHive,
    hash_reader,
};

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsPrerequisiteDetector;

impl PrerequisiteDetector for WindowsPrerequisiteDetector {
    fn detect(&self, operation: &BootstrapOperation) -> Result<DetectionResult, BootstrapError> {
        match &operation.detector {
            PrerequisiteDetectorSpec::VisualCppV14 { version } => detect_registry_version(
                RegistryHive::LocalMachine,
                &format!(
                    "SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\{}",
                    architecture_name(operation.target)
                ),
                "Version",
                version.as_ref(),
                operation.target,
            ),
            PrerequisiteDetectorSpec::DotNetRuntime { desktop, version } => {
                let runtime = if *desktop {
                    "WindowsDesktop"
                } else {
                    "NetCore"
                };
                detect_dotnet(operation.target, *desktop, version.as_ref(), runtime)
            }
            PrerequisiteDetectorSpec::WebView2Evergreen { version } => {
                let path = match operation.target {
                    PrerequisiteArchitecture::X86 => {
                        "SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8A7C-971BF3207148}"
                    }
                    PrerequisiteArchitecture::X64 | PrerequisiteArchitecture::Arm64 => {
                        "SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8A7C-971BF3207148}"
                    }
                    PrerequisiteArchitecture::Current | PrerequisiteArchitecture::Any => {
                        "SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8A7C-971BF3207148}"
                    }
                };
                detect_registry_version(
                    RegistryHive::LocalMachine,
                    path,
                    "pv",
                    version.as_ref(),
                    operation.target,
                )
            }
            PrerequisiteDetectorSpec::MsiProduct {
                product_code,
                version,
            } => detect_msi(product_code, version.as_ref()),
            PrerequisiteDetectorSpec::RegistryValue {
                hive,
                key,
                value,
                version,
                expected,
            } => detect_registry_value(
                *hive,
                key,
                value,
                version.as_ref(),
                expected.as_deref(),
                operation.target,
            ),
            PrerequisiteDetectorSpec::FileVersion { path, version } => {
                let path = path.as_literal().ok_or_else(|| {
                    BootstrapError::Detector("unresolved file-version path".into())
                })?;
                detect_file_version(Path::new(path), version.as_ref())
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct WindowsPrerequisiteProvider;

impl PrerequisiteProvider for WindowsPrerequisiteProvider {
    fn execute(&self, request: &ProviderRequest) -> Result<ProviderOutcome, BootstrapError> {
        let _artifact = verify_artifact(
            &request.executable,
            request.expected_digest,
            request.expected_size,
        )
        .map_err(|error| BootstrapError::ProviderPreflight(error.to_string()))?;
        let (program, arguments) = match request.installer_kind {
            zup_core::PrerequisiteInstallerKind::Msi => {
                let msiexec = system_directory()
                    .map_err(|error| BootstrapError::ProviderPreflight(error.to_string()))?
                    .join("msiexec.exe");
                if !msiexec.is_absolute() {
                    return Err(BootstrapError::ProviderPreflight(
                        "msiexec path is not absolute".into(),
                    ));
                }
                let mut arguments = vec![
                    "/i".to_owned(),
                    request.executable.display().to_string(),
                    "/qn".to_owned(),
                ];
                arguments.extend(request.arguments.iter().cloned());
                arguments.push("/norestart".to_owned());
                (msiexec, arguments)
            }
            zup_core::PrerequisiteInstallerKind::Exe => {
                let mut arguments = match request.builtin {
                    BuiltinProvider::VisualCppV14 => {
                        vec!["/install".to_owned(), "/quiet".to_owned()]
                    }
                    BuiltinProvider::WebView2Evergreen => {
                        vec!["/silent".to_owned(), "/install".to_owned()]
                    }
                    BuiltinProvider::DotNetRuntime => {
                        vec!["/install".to_owned(), "/quiet".to_owned()]
                    }
                    BuiltinProvider::Msi | BuiltinProvider::ExplicitExe => Vec::new(),
                };
                arguments.extend(request.arguments.iter().cloned());
                if !arguments
                    .iter()
                    .any(|argument| argument.eq_ignore_ascii_case("/norestart"))
                {
                    arguments.push("/norestart".to_owned());
                }
                (request.executable.clone(), arguments)
            }
        };
        if !program.is_absolute() {
            return Err(BootstrapError::ProviderPreflight(
                "prerequisite executable path is not absolute".into(),
            ));
        }
        let status = Command::new(&program)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                BootstrapError::ProviderPreflight(format!("start prerequisite: {error}"))
            })?;
        let code = status.code().ok_or_else(|| {
            BootstrapError::Provider("prerequisite terminated without an exit code".into())
        })?;
        if request.success_exit_codes.contains(&code) {
            Ok(ProviderOutcome::Succeeded)
        } else if code == 1641 || request.reboot_exit_codes.contains(&code) {
            Ok(ProviderOutcome::RebootRequired { exit_code: code })
        } else {
            Err(BootstrapError::Provider(format!(
                "prerequisite exited with code {code}"
            )))
        }
    }
}

#[cfg(windows)]
fn system_directory() -> Result<PathBuf, BootstrapError> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = [0u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(BootstrapError::Provider(
            "could not resolve the Windows system directory".into(),
        ));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length as usize],
    )))
}

#[cfg(not(windows))]
fn system_directory() -> Result<PathBuf, BootstrapError> {
    Err(BootstrapError::Provider(
        "MSI prerequisites require Windows".into(),
    ))
}

fn verify_artifact(
    path: &Path,
    expected_digest: zup_core::Sha256Digest,
    expected_size: Option<u64>,
) -> Result<std::fs::File, BootstrapError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| BootstrapError::Provider(format!("artifact metadata: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BootstrapError::Provider(
            "artifact is not a regular file".into(),
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    let file = options
        .open(path)
        .map_err(|error| BootstrapError::Provider(format!("artifact open: {error}")))?;
    let (size, digest) = hash_reader(&file)
        .map_err(|error| BootstrapError::Provider(format!("artifact hash: {error}")))?;
    if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES
        || digest != expected_digest
        || expected_size.is_some_and(|expected| expected != size)
    {
        return Err(BootstrapError::Provider(
            "artifact digest or size changed before execution".into(),
        ));
    }
    Ok(file)
}

fn detect_registry_version(
    hive: RegistryHive,
    path: &str,
    value_name: &str,
    requirement: Option<&VersionReq>,
    architecture: PrerequisiteArchitecture,
) -> Result<DetectionResult, BootstrapError> {
    let Some(key) = open_registry_key(hive, path, architecture)? else {
        return Ok(DetectionResult::Missing);
    };
    let Some(value) = read_version_value(&key, value_name)? else {
        return Ok(DetectionResult::Missing);
    };
    let Some(version) = parse_runtime_version(&value) else {
        return Ok(DetectionResult::Missing);
    };
    if requirement.is_some_and(|requirement| !requirement.matches(&version)) {
        return Ok(DetectionResult::Incompatible {
            found: version,
            required: requirement.expect("checked above").clone(),
        });
    }
    Ok(DetectionResult::Satisfied {
        version: Some(version),
        evidence: format!("registry {path}\\{value_name}"),
    })
}

fn detect_dotnet(
    architecture: PrerequisiteArchitecture,
    desktop: bool,
    requirement: Option<&VersionReq>,
    runtime: &str,
) -> Result<DetectionResult, BootstrapError> {
    let architecture_name = architecture_name(architecture);
    let path = format!(
        "SOFTWARE\\dotnet\\Setup\\InstalledVersions\\{architecture_name}\\sharedfx\\Microsoft\\{runtime}"
    );
    let Some(key) = open_registry_key(RegistryHive::LocalMachine, &path, architecture)? else {
        return Ok(DetectionResult::Missing);
    };
    let Some(value) = read_version_value(&key, "Version")? else {
        return Ok(DetectionResult::Missing);
    };
    let Some(version) = parse_runtime_version(&value) else {
        return Ok(DetectionResult::Missing);
    };
    if requirement.is_some_and(|requirement| !requirement.matches(&version)) {
        return Ok(DetectionResult::Incompatible {
            found: version,
            required: requirement.expect("checked above").clone(),
        });
    }
    let _ = desktop;
    Ok(DetectionResult::Satisfied {
        version: Some(version),
        evidence: format!("registry {path}\\Version"),
    })
}

fn detect_registry_value(
    hive: RegistryHive,
    path: &str,
    value_name: &str,
    requirement: Option<&VersionReq>,
    expected: Option<&str>,
    architecture: PrerequisiteArchitecture,
) -> Result<DetectionResult, BootstrapError> {
    let Some(key) = open_registry_key(hive, path, architecture)? else {
        return Ok(DetectionResult::Missing);
    };
    let value = match key.get_value(value_name) {
        Ok(value) => value,
        Err(error) if registry_not_found(&error) => return Ok(DetectionResult::Missing),
        Err(error) => {
            return Err(BootstrapError::Detector(format!(
                "registry value {path}\\{value_name}: {error}"
            )));
        }
    };
    let text = match value.ty() {
        Type::String | Type::ExpandString => String::try_from(value).ok(),
        Type::U32 => u32::try_from(value).ok().map(|value| value.to_string()),
        Type::U64 => u64::try_from(value).ok().map(|value| value.to_string()),
        _ => None,
    };
    let Some(text) = text else {
        return Ok(DetectionResult::Missing);
    };
    if let Some(expected) = expected
        && !text.eq_ignore_ascii_case(expected)
    {
        return Ok(DetectionResult::Missing);
    }
    if let Some(requirement) = requirement {
        let Some(version) = parse_runtime_version(&text) else {
            return Ok(DetectionResult::Missing);
        };
        if !requirement.matches(&version) {
            return Ok(DetectionResult::Incompatible {
                found: version,
                required: requirement.clone(),
            });
        }
        return Ok(DetectionResult::Satisfied {
            version: Some(version),
            evidence: format!("registry {path}\\{value_name}"),
        });
    }
    Ok(DetectionResult::Satisfied {
        version: None,
        evidence: format!("registry {path}\\{value_name}"),
    })
}

fn detect_msi(
    product_code: &str,
    requirement: Option<&VersionReq>,
) -> Result<DetectionResult, BootstrapError> {
    let product: Vec<u16> = product_code.encode_utf16().chain(Some(0)).collect();
    let state = unsafe {
        windows::Win32::System::ApplicationInstallationAndServicing::MsiQueryProductStateW(
            windows::core::PCWSTR(product.as_ptr()),
        )
    };
    if state != windows::Win32::System::ApplicationInstallationAndServicing::INSTALLSTATE_LOCAL
        && state
            != windows::Win32::System::ApplicationInstallationAndServicing::INSTALLSTATE_DEFAULT
    {
        return Ok(DetectionResult::Missing);
    }
    if let Some(requirement) = requirement {
        let attribute: Vec<u16> = "VersionString".encode_utf16().chain(Some(0)).collect();
        let mut length = 0u32;
        let code = unsafe {
            windows::Win32::System::ApplicationInstallationAndServicing::MsiGetProductInfoW(
                windows::core::PCWSTR(product.as_ptr()),
                windows::core::PCWSTR(attribute.as_ptr()),
                None,
                Some(&mut length),
            )
        };
        if code != 0 || length == 0 {
            return Err(BootstrapError::Detector(format!(
                "MSI product query failed for {product_code}"
            )));
        }
        let mut buffer = vec![0u16; length as usize];
        let code = unsafe {
            windows::Win32::System::ApplicationInstallationAndServicing::MsiGetProductInfoW(
                windows::core::PCWSTR(product.as_ptr()),
                windows::core::PCWSTR(attribute.as_ptr()),
                Some(windows::core::PWSTR(buffer.as_mut_ptr())),
                Some(&mut length),
            )
        };
        if code != 0 {
            return Err(BootstrapError::Detector(format!(
                "MSI version query failed for {product_code}"
            )));
        }
        let text = String::from_utf16_lossy(&buffer[..length.saturating_sub(1) as usize]);
        let Some(version) = parse_runtime_version(&text) else {
            return Ok(DetectionResult::Missing);
        };
        if !requirement.matches(&version) {
            return Ok(DetectionResult::Incompatible {
                found: version,
                required: requirement.clone(),
            });
        }
        return Ok(DetectionResult::Satisfied {
            version: Some(version),
            evidence: format!("MSI product {product_code}"),
        });
    }
    Ok(DetectionResult::Satisfied {
        version: None,
        evidence: format!("MSI product {product_code}"),
    })
}

fn detect_file_version(
    path: &Path,
    requirement: Option<&VersionReq>,
) -> Result<DetectionResult, BootstrapError> {
    let Some(version) = read_file_version(path)? else {
        return Ok(DetectionResult::Missing);
    };
    if let Some(requirement) = requirement
        && !requirement.matches(&version)
    {
        return Ok(DetectionResult::Incompatible {
            found: version,
            required: requirement.clone(),
        });
    }
    Ok(DetectionResult::Satisfied {
        version: Some(version),
        evidence: path.display().to_string(),
    })
}

fn open_registry_key(
    hive: RegistryHive,
    path: &str,
    architecture: PrerequisiteArchitecture,
) -> Result<Option<Key>, BootstrapError> {
    let root = match hive {
        RegistryHive::CurrentUser => windows_registry::CURRENT_USER,
        RegistryHive::LocalMachine => LOCAL_MACHINE,
        RegistryHive::ClassesRoot => windows_registry::CLASSES_ROOT,
    };
    let mut options = root.options();
    options.read();
    match architecture {
        PrerequisiteArchitecture::X86 => {
            options.wow64_32();
        }
        PrerequisiteArchitecture::X64 | PrerequisiteArchitecture::Arm64 => {
            options.wow64_64();
        }
        PrerequisiteArchitecture::Current | PrerequisiteArchitecture::Any => {}
    }
    match options.open(path) {
        Ok(key) => Ok(Some(key)),
        Err(error) if registry_not_found(&error) => Ok(None),
        Err(error) => Err(BootstrapError::Detector(format!(
            "registry open {path}: {error}"
        ))),
    }
}

fn registry_not_found(error: &windows_result::Error) -> bool {
    let code = error.code().0 as u32;
    matches!(code & 0xffff, 2 | 3)
}

fn read_version_value(key: &Key, name: &str) -> Result<Option<String>, BootstrapError> {
    let value = match key.get_value(name) {
        Ok(value) => value,
        Err(error) if registry_not_found(&error) => return Ok(None),
        Err(error) => {
            return Err(BootstrapError::Detector(format!(
                "registry value {name}: {error}"
            )));
        }
    };
    match value.ty() {
        Type::String | Type::ExpandString => Ok(String::try_from(value).ok()),
        Type::U32 => Ok(u32::try_from(value).ok().map(|value| value.to_string())),
        Type::U64 => Ok(u64::try_from(value).ok().map(|value| value.to_string())),
        _ => Ok(None),
    }
}

fn architecture_name(architecture: PrerequisiteArchitecture) -> &'static str {
    match architecture {
        PrerequisiteArchitecture::X86 => "x86",
        PrerequisiteArchitecture::X64 => "x64",
        PrerequisiteArchitecture::Arm64 => "arm64",
        PrerequisiteArchitecture::Current | PrerequisiteArchitecture::Any => {
            if cfg!(target_arch = "x86") {
                "x86"
            } else if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            }
        }
    }
}

fn parse_runtime_version(value: &str) -> Option<Version> {
    let mut parts = value.trim().split('.').filter(|part| !part.is_empty());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some(Version::new(major, minor, patch))
}

#[cfg(windows)]
fn read_file_version(path: &Path) -> Result<Option<Version>, BootstrapError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    use windows::core::PCWSTR;

    #[repr(C)]
    struct VsFixedFileInfo {
        signature: u32,
        struct_version: u32,
        file_version_ms: u32,
        file_version_ls: u32,
        product_version_ms: u32,
        product_version_ls: u32,
        file_flags_mask: u32,
        file_flags: u32,
        file_os: u32,
        file_type: u32,
        file_subtype: u32,
        file_date_ms: u32,
        file_date_ls: u32,
    }

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None) };
    if size == 0 {
        return Ok(None);
    }
    let mut data = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr().cast()) }
        .map_err(|error| BootstrapError::Detector(format!("file version query: {error}")))?;
    let query: Vec<u16> = "\\".encode_utf16().chain(Some(0)).collect();
    let mut info = std::ptr::null_mut();
    let mut length = 0u32;
    let ok = unsafe {
        VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut info,
            &mut length,
        )
    };
    if !ok.as_bool() || info.is_null() || (length as usize) < std::mem::size_of::<VsFixedFileInfo>()
    {
        return Ok(None);
    }
    let info = unsafe { &*info.cast::<VsFixedFileInfo>() };
    if info.signature != 0xFEEF_04BD {
        return Ok(None);
    }
    let major = (info.file_version_ms >> 16) as u64;
    let minor = (info.file_version_ms & 0xffff) as u64;
    let build = (info.file_version_ls >> 16) as u64;
    Ok(Some(Version::new(major, minor, build)))
}

#[cfg(not(windows))]
fn read_file_version(_path: &Path) -> Result<Option<Version>, BootstrapError> {
    Ok(None)
}
