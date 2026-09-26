//! Windows prerequisite detection and package execution.
//!
//! Portable requirements carry opaque ids. This module owns the stable ids it
//! can answer, the registry and package queries behind them, and the command
//! line used to install a verified artifact.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use semver::{Version, VersionReq};
use windows_registry::{Key, Type};
use zup_bootstrap::{
    BootstrapError, BootstrapOperation, DetectionResult, PrerequisiteProvider,
    PrerequisiteSatisfier, ProviderOutcome, ProviderRequest,
};
use zup_core::{
    FileVersion, InstalledPackage, PrerequisiteArchitecture, PrerequisiteRequirement, Runtime,
};

/// Stable runtime requirement ids understood by the Windows adapter.
pub mod runtime_requirements {
    /// Visual C++ 2015-2022 redistributable runtime (`14.x`).
    pub const VISUAL_CPP_V14: &str = "windows.vc.v14";
    /// .NET Desktop Runtime.
    pub const DOTNET_DESKTOP: &str = "windows.dotnet.desktop";
    /// .NET Runtime.
    pub const DOTNET_RUNTIME: &str = "windows.dotnet.runtime";
    /// Microsoft Edge WebView2 Evergreen Runtime.
    pub const WEBVIEW2_EVERGREEN: &str = "windows.webview2.evergreen";
}

/// Stable installed-package ids understood by the Windows adapter.
pub mod package_requirements {
    /// Microsoft Edge WebView2 Evergreen Bootstrapper product code.
    pub const WEBVIEW2_BOOTSTRAPPER: &str = "{F3017226-FE2A-4295-8A7C-971BF3207148}";
}

/// WebView2 Evergreen Runtime client registry key.
const WEBVIEW2_CLIENT_KEY: &str =
    "SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8A7C-971BF3207148}";
/// WebView2 Evergreen Runtime client registry key as seen by 32-bit processes.
const WEBVIEW2_CLIENT_KEY_WOW64: &str =
    "SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8A7C-971BF3207148}";
/// The Visual C++ redistributable runtime reports its version per architecture.
const VISUAL_CPP_V14_KEY: &str = "SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes";
/// The .NET installer records installed shared frameworks per architecture.
const DOTNET_INSTALLED_VERSIONS_KEY: &str = "SOFTWARE\\dotnet\\Setup\\InstalledVersions";

/// Compound-file (CFB) header: the on-disk format of every Windows Installer package.
const COMPOUND_FILE_MAGIC: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

/// How a verified prerequisite artifact is launched on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactFormat {
    /// Windows Installer package, installed through `msiexec`.
    WindowsInstaller,
    /// Ordinary executable, launched directly.
    Executable,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsPrerequisiteDetector;

impl PrerequisiteSatisfier for WindowsPrerequisiteDetector {
    fn satisfy(&self, operation: &BootstrapOperation) -> Result<DetectionResult, BootstrapError> {
        match &operation.requirement {
            PrerequisiteRequirement::Runtime(runtime) => satisfy_runtime(operation, runtime),
            PrerequisiteRequirement::InstalledPackage(package) => {
                satisfy_installed_package(package)
            }
            PrerequisiteRequirement::FileVersion(file) => satisfy_file_version(file),
        }
    }
}

fn satisfy_runtime(
    operation: &BootstrapOperation,
    runtime: &Runtime,
) -> Result<DetectionResult, BootstrapError> {
    let id = runtime.id.as_str();
    let requirement = runtime.version.as_ref();
    let evidence = || format!("runtime {id}");
    if id == runtime_requirements::VISUAL_CPP_V14 {
        return detect_version(
            &format!(
                "{VISUAL_CPP_V14_KEY}\\{}",
                architecture_name(operation.target)
            ),
            "Version",
            requirement,
            operation.target,
            evidence(),
        );
    }
    if id == runtime_requirements::WEBVIEW2_EVERGREEN {
        let path = match operation.target {
            PrerequisiteArchitecture::X64 | PrerequisiteArchitecture::Arm64 => WEBVIEW2_CLIENT_KEY,
            _ => WEBVIEW2_CLIENT_KEY_WOW64,
        };
        return detect_version(path, "pv", requirement, operation.target, evidence());
    }
    if id == runtime_requirements::DOTNET_DESKTOP {
        return detect_dotnet(operation.target, "WindowsDesktop", requirement, evidence());
    }
    if id == runtime_requirements::DOTNET_RUNTIME {
        return detect_dotnet(operation.target, "NetCore", requirement, evidence());
    }
    Err(BootstrapError::Requirement(format!(
        "no Windows runtime is registered for requirement id `{id}`"
    )))
}

fn detect_dotnet(
    architecture: PrerequisiteArchitecture,
    runtime: &str,
    requirement: Option<&VersionReq>,
    evidence: String,
) -> Result<DetectionResult, BootstrapError> {
    detect_version(
        &format!(
            "{DOTNET_INSTALLED_VERSIONS_KEY}\\{}\\sharedfx\\Microsoft\\{runtime}",
            architecture_name(architecture)
        ),
        "Version",
        requirement,
        architecture,
        evidence,
    )
}

fn detect_version(
    path: &str,
    value_name: &str,
    requirement: Option<&VersionReq>,
    architecture: PrerequisiteArchitecture,
    evidence: String,
) -> Result<DetectionResult, BootstrapError> {
    let Some(key) = open_machine_key(path, architecture)? else {
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
        evidence,
    })
}

fn satisfy_installed_package(
    package: &InstalledPackage,
) -> Result<DetectionResult, BootstrapError> {
    let product_code = package.id.as_str();
    let requirement = package.version.as_ref();
    let state = unsafe {
        windows::Win32::System::ApplicationInstallationAndServicing::MsiQueryProductStateW(
            windows::core::PCWSTR(wide(product_code).as_ptr()),
        )
    };
    if state != windows::Win32::System::ApplicationInstallationAndServicing::INSTALLSTATE_LOCAL
        && state
            != windows::Win32::System::ApplicationInstallationAndServicing::INSTALLSTATE_DEFAULT
    {
        return Ok(DetectionResult::Missing);
    }
    if requirement.is_none() {
        return Ok(DetectionResult::Satisfied {
            version: None,
            evidence: format!("package {product_code}"),
        });
    }
    let text = product_version_string(product_code)?;
    let Some(version) = parse_runtime_version(&text) else {
        return Ok(DetectionResult::Missing);
    };
    if !requirement.expect("checked above").matches(&version) {
        return Ok(DetectionResult::Incompatible {
            found: version,
            required: requirement.expect("checked above").clone(),
        });
    }
    Ok(DetectionResult::Satisfied {
        version: Some(version),
        evidence: format!("package {product_code}"),
    })
}

fn product_version_string(product_code: &str) -> Result<String, BootstrapError> {
    let product = wide(product_code);
    let attribute = wide("VersionString");
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
        return Err(BootstrapError::Requirement(format!(
            "package {product_code} did not report a version"
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
        return Err(BootstrapError::Requirement(format!(
            "package {product_code} version query failed"
        )));
    }
    Ok(String::from_utf16_lossy(
        &buffer[..length.saturating_sub(1) as usize],
    ))
}

fn satisfy_file_version(file: &FileVersion) -> Result<DetectionResult, BootstrapError> {
    let Some(path) = file.path.as_literal() else {
        return Err(BootstrapError::Requirement(
            "unresolved file-version path".into(),
        ));
    };
    let Some(version) = read_file_version(Path::new(path))? else {
        return Ok(DetectionResult::Missing);
    };
    if let Some(requirement) = file.version.as_ref()
        && !requirement.matches(&version)
    {
        return Ok(DetectionResult::Incompatible {
            found: version,
            required: requirement.clone(),
        });
    }
    Ok(DetectionResult::Satisfied {
        version: Some(version),
        evidence: path.to_owned(),
    })
}

#[derive(Debug, Default, Clone)]
pub struct WindowsPrerequisiteProvider;

impl PrerequisiteProvider for WindowsPrerequisiteProvider {
    fn execute(&self, request: &ProviderRequest) -> Result<ProviderOutcome, BootstrapError> {
        let format = verify_artifact(
            &request.executable,
            request.expected_digest,
            request.expected_size,
        )
        .map_err(|error| BootstrapError::ProviderPreflight(error.to_string()))?;
        let launch = plan_launch(request, format)?;
        let status = Command::new(&launch.program)
            .args(&launch.arguments)
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

/// The command line used to install one verified artifact.
struct Launch {
    program: PathBuf,
    arguments: Vec<String>,
}

fn plan_launch(
    request: &ProviderRequest,
    format: ArtifactFormat,
) -> Result<Launch, BootstrapError> {
    let launch = match format {
        ArtifactFormat::WindowsInstaller => {
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
            Launch {
                program: msiexec,
                arguments,
            }
        }
        ArtifactFormat::Executable => {
            let mut arguments = default_arguments(&request.requirement);
            arguments.extend(request.arguments.iter().cloned());
            if !arguments
                .iter()
                .any(|argument| argument.eq_ignore_ascii_case("/norestart"))
            {
                arguments.push("/norestart".to_owned());
            }
            Launch {
                program: request.executable.clone(),
                arguments,
            }
        }
    };
    if !launch.program.is_absolute() {
        return Err(BootstrapError::ProviderPreflight(
            "prerequisite executable path is not absolute".into(),
        ));
    }
    Ok(launch)
}

/// Provider-owned silent arguments for runtimes with a known installer contract.
fn default_arguments(requirement: &PrerequisiteRequirement) -> Vec<String> {
    let PrerequisiteRequirement::Runtime(runtime) = requirement else {
        return Vec::new();
    };
    match runtime.id.as_str() {
        runtime_requirements::VISUAL_CPP_V14 | runtime_requirements::DOTNET_DESKTOP => {
            vec!["/install".to_owned(), "/quiet".to_owned()]
        }
        runtime_requirements::DOTNET_RUNTIME => vec!["/install".to_owned(), "/quiet".to_owned()],
        runtime_requirements::WEBVIEW2_EVERGREEN => {
            vec!["/silent".to_owned(), "/install".to_owned()]
        }
        _ => Vec::new(),
    }
}

/// Verify the artifact still matches its declared identity and classify its format.
fn verify_artifact(
    path: &Path,
    expected_digest: zup_core::Sha256Digest,
    expected_size: Option<u64>,
) -> Result<ArtifactFormat, BootstrapError> {
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
    let mut file = options
        .open(path)
        .map_err(|error| BootstrapError::Provider(format!("artifact open: {error}")))?;
    let (size, digest) = zup_core::hash_reader(&file)
        .map_err(|error| BootstrapError::Provider(format!("artifact hash: {error}")))?;
    if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES
        || digest != expected_digest
        || expected_size.is_some_and(|expected| expected != size)
    {
        return Err(BootstrapError::Provider(
            "artifact digest or size changed before execution".into(),
        ));
    }
    Ok(classify_artifact(&mut file))
}

fn classify_artifact(file: &mut std::fs::File) -> ArtifactFormat {
    use std::io::Seek;
    if file.seek(std::io::SeekFrom::Start(0)).is_err() {
        return ArtifactFormat::Executable;
    }
    let mut magic = [0u8; COMPOUND_FILE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) if magic == COMPOUND_FILE_MAGIC => ArtifactFormat::WindowsInstaller,
        _ => ArtifactFormat::Executable,
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
        "Windows Installer packages require Windows".into(),
    ))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn open_machine_key(
    path: &str,
    architecture: PrerequisiteArchitecture,
) -> Result<Option<Key>, BootstrapError> {
    let mut options = windows_registry::LOCAL_MACHINE.options();
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
        Err(error) => Err(BootstrapError::Requirement(format!(
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
            return Err(BootstrapError::Requirement(format!(
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

    let encoded = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(encoded.as_ptr()), None) };
    if size == 0 {
        return Ok(None);
    }
    let mut data = vec![0u8; size as usize];
    unsafe {
        GetFileVersionInfoW(
            PCWSTR(encoded.as_ptr()),
            None,
            size,
            data.as_mut_ptr().cast(),
        )
    }
    .map_err(|error| BootstrapError::Requirement(format!("file version query: {error}")))?;
    let query = wide("\\");
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use zup_core::{
        InstalledPackage, InstalledPackageId, PrerequisiteId, Runtime, RuntimeRequirementId,
        Sha256Digest,
    };

    use super::*;

    const PE_MAGIC: &[u8; 2] = b"MZ";

    fn digest(bytes: &[u8]) -> Sha256Digest {
        zup_core::hash_reader(bytes).unwrap().1
    }

    fn compound_file_bytes() -> Vec<u8> {
        let mut bytes = COMPOUND_FILE_MAGIC.to_vec();
        bytes.extend_from_slice(&[0u8; 512]);
        bytes
    }

    fn artifact(bytes: &[u8], name: &str) -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        let path = root.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (root, path)
    }

    fn runtime_requirement(id: &str) -> PrerequisiteRequirement {
        PrerequisiteRequirement::Runtime(Runtime {
            id: RuntimeRequirementId::new(id).unwrap(),
            version: None,
        })
    }

    fn provider_request(
        executable: PathBuf,
        requirement: PrerequisiteRequirement,
    ) -> ProviderRequest {
        ProviderRequest {
            prerequisite_id: PrerequisiteId::new("runtime").unwrap(),
            requirement,
            executable,
            arguments: Vec::new(),
            expected_digest: Sha256Digest::from_bytes([0u8; 32]),
            expected_size: None,
            success_exit_codes: vec![0],
            reboot_exit_codes: vec![1641, 3010],
        }
    }

    #[test]
    fn windows_owned_ids_are_valid_portable_requirement_ids() {
        for id in [
            runtime_requirements::VISUAL_CPP_V14,
            runtime_requirements::DOTNET_DESKTOP,
            runtime_requirements::DOTNET_RUNTIME,
            runtime_requirements::WEBVIEW2_EVERGREEN,
        ] {
            assert!(
                RuntimeRequirementId::new(id).is_ok(),
                "`{id}` must be usable"
            );
        }
        assert!(InstalledPackageId::new(package_requirements::WEBVIEW2_BOOTSTRAPPER).is_ok());
    }

    #[test]
    fn artifact_format_comes_from_verified_content() {
        let package = compound_file_bytes();
        let (_root, path) = artifact(&package, "runtime.msi");
        assert_eq!(
            verify_artifact(&path, digest(&package), Some(package.len() as u64)).unwrap(),
            ArtifactFormat::WindowsInstaller
        );
        let (_root, tampered) = artifact(b"not the declared package", "runtime.msi");
        assert!(verify_artifact(&tampered, digest(&package), None).is_err());
        assert!(verify_artifact(&path, digest(&package), Some(1)).is_err());

        let executable = [PE_MAGIC.as_slice(), &[0u8; 64]].concat();
        let (_root, path) = artifact(&executable, "runtime.exe");
        assert_eq!(
            verify_artifact(&path, digest(&executable), Some(executable.len() as u64)).unwrap(),
            ArtifactFormat::Executable
        );
    }

    #[test]
    fn windows_installer_packages_run_through_msiexec_silently() {
        let package = compound_file_bytes();
        let (_root, path) = artifact(&package, "runtime.msi");
        let mut request = provider_request(path.clone(), runtime_requirement("windows.vc.v14"));
        request.arguments = vec!["ALLUSERS=1".to_owned()];
        let launch = plan_launch(&request, ArtifactFormat::WindowsInstaller).unwrap();
        assert_eq!(
            launch.program,
            system_directory().unwrap().join("msiexec.exe")
        );
        assert_eq!(
            launch.arguments,
            [
                "/i".to_owned(),
                path.display().to_string(),
                "/qn".to_owned(),
                "ALLUSERS=1".to_owned(),
                "/norestart".to_owned(),
            ]
        );
    }

    #[test]
    fn executables_keep_their_own_silent_contract_and_run_directly() {
        let package = compound_file_bytes();
        let (_root, path) = artifact(&package, "runtime.exe");
        let launch = plan_launch(
            &provider_request(
                path.clone(),
                runtime_requirement(runtime_requirements::VISUAL_CPP_V14),
            ),
            ArtifactFormat::Executable,
        )
        .unwrap();
        assert_eq!(launch.program, path);
        assert_eq!(launch.arguments, ["/install", "/quiet", "/norestart"]);

        let launch = plan_launch(
            &provider_request(
                path.clone(),
                runtime_requirement(runtime_requirements::WEBVIEW2_EVERGREEN),
            ),
            ArtifactFormat::Executable,
        )
        .unwrap();
        assert_eq!(launch.arguments, ["/silent", "/install", "/norestart"]);

        let mut request = provider_request(
            path.clone(),
            runtime_requirement(runtime_requirements::DOTNET_DESKTOP),
        );
        request.arguments = vec!["/norestart".to_owned()];
        let launch = plan_launch(&request, ArtifactFormat::Executable).unwrap();
        assert_eq!(launch.arguments, ["/install", "/quiet", "/norestart"]);

        let request = provider_request(
            path.clone(),
            PrerequisiteRequirement::InstalledPackage(InstalledPackage {
                id: InstalledPackageId::new(package_requirements::WEBVIEW2_BOOTSTRAPPER).unwrap(),
                version: None,
            }),
        );
        let launch = plan_launch(&request, ArtifactFormat::Executable).unwrap();
        assert_eq!(launch.arguments, ["/norestart"]);
    }

    #[test]
    fn unknown_runtime_ids_are_rejected_instead_of_reported_missing() {
        let operation = BootstrapOperation {
            id: PrerequisiteId::new("runtime").unwrap(),
            name: "Runtime".into(),
            target: PrerequisiteArchitecture::Current,
            requirement: runtime_requirement("windows.vc.v99"),
            package: zup_core::PrerequisitePackage::Remote {
                url: "https://cdn.example.test/vc.exe".into(),
                sha256: digest(b"vc"),
                size: Some(2),
                filename: "vc.exe".into(),
            },
            installer: zup_core::PrerequisiteInstaller::default(),
        };
        assert!(matches!(
            WindowsPrerequisiteDetector.satisfy(&operation),
            Err(BootstrapError::Requirement(_))
        ));
    }

    #[test]
    fn an_absent_installed_package_is_missing_rather_than_an_error() {
        let requirement = PrerequisiteRequirement::InstalledPackage(InstalledPackage {
            id: InstalledPackageId::new("{00000000-0000-0000-0000-000000000000}").unwrap(),
            version: None,
        });
        let operation = BootstrapOperation {
            id: PrerequisiteId::new("runtime").unwrap(),
            name: "Runtime".into(),
            target: PrerequisiteArchitecture::Current,
            requirement,
            package: zup_core::PrerequisitePackage::Remote {
                url: "https://cdn.example.test/pkg.msi".into(),
                sha256: digest(b"pkg"),
                size: Some(3),
                filename: "pkg.msi".into(),
            },
            installer: zup_core::PrerequisiteInstaller::default(),
        };
        assert_eq!(
            WindowsPrerequisiteDetector.satisfy(&operation).unwrap(),
            DetectionResult::Missing
        );
    }
}
