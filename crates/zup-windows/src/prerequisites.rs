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

pub mod runtime_requirements {

    pub const VISUAL_CPP_V14: &str = "windows.vc.v14";

    pub const DOTNET_DESKTOP: &str = "windows.dotnet.desktop";

    pub const DOTNET_RUNTIME: &str = "windows.dotnet.runtime";

    pub const WEBVIEW2_EVERGREEN: &str = "windows.webview2.evergreen";
}

pub mod package_requirements {

    pub const WEBVIEW2_BOOTSTRAPPER: &str = "{F3017226-FE2A-4295-8A7C-971BF3207148}";
}

const WEBVIEW2_CLIENT_KEY: &str =
    "SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8A7C-971BF3207148}";

const VISUAL_CPP_V14_KEY: &str = "SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes";

const DOTNET_INSTALLED_VERSIONS_KEY: &str = "SOFTWARE\\dotnet\\Setup\\InstalledVersions";

const COMPOUND_FILE_MAGIC: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactFormat {
    WindowsInstaller,

    Executable,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowsPrerequisiteDetector {
    scope: zup_core::SelectedScope,
}
impl WindowsPrerequisiteDetector {
    pub fn new(scope: zup_core::SelectedScope) -> Self {
        Self { scope }
    }
}

impl PrerequisiteSatisfier for WindowsPrerequisiteDetector {
    fn satisfy(&self, operation: &BootstrapOperation) -> Result<DetectionResult, BootstrapError> {
        match &operation.requirement {
            PrerequisiteRequirement::NativePackage { .. } => Err(BootstrapError::Requirement(
                "native Linux packages are not supported on Windows".into(),
            )),
            PrerequisiteRequirement::Runtime(runtime) => {
                satisfy_runtime(operation, runtime, self.scope)
            }
            PrerequisiteRequirement::InstalledPackage(package) => {
                satisfy_installed_package(package, operation.target, self.scope)
            }
            PrerequisiteRequirement::FileVersion(file) => satisfy_file_version(file),
        }
    }
}

fn satisfy_runtime(
    operation: &BootstrapOperation,
    runtime: &Runtime,
    scope: zup_core::SelectedScope,
) -> Result<DetectionResult, BootstrapError> {
    if runtime.id.as_str() == runtime_requirements::VISUAL_CPP_V14
        && operation.target == PrerequisiteArchitecture::Any
    {
        let mut versions = Vec::new();
        for target in supported_architectures() {
            let mut operation = operation.clone();
            operation.target = target;
            match satisfy_runtime(&operation, runtime, scope)? {
                result @ DetectionResult::Satisfied { .. } => return Ok(result),
                DetectionResult::Incompatible { found, .. } => versions.push(found),
                DetectionResult::Missing => {}
            }
        }
        return select_version(versions, runtime.version.as_ref(), "VC++ v14".into());
    }
    if operation.target != PrerequisiteArchitecture::Any
        && !supported_architectures()
            .iter()
            .any(|arch| architecture_name(*arch) == architecture_name(operation.target))
    {
        return Err(BootstrapError::Requirement(
            "runtime architecture is unsupported by this OS".into(),
        ));
    }
    let id = runtime.id.as_str();
    let requirement = runtime.version.as_ref();
    let evidence = || format!("runtime {id}");
    if id == runtime_requirements::VISUAL_CPP_V14 {
        let path = format!(
            "{VISUAL_CPP_V14_KEY}\\{}",
            architecture_name(operation.target)
        );
        let Some(key) = open_machine_key(&path, operation.target)? else {
            return Ok(DetectionResult::Missing);
        };
        match key.get_u32("Installed").map_err(|error| {
            BootstrapError::Requirement(format!("VC++ installed marker: {error}"))
        })? {
            0 => return Ok(DetectionResult::Missing),
            1 => {}
            _ => {
                return Err(BootstrapError::Requirement(
                    "invalid VC++ installed marker".into(),
                ));
            }
        }
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
        if operation.target != PrerequisiteArchitecture::Any {
            return Err(BootstrapError::Requirement("Evergreen platform registration does not prove a runtime architecture; use target any".into()));
        }
        let machine = detect_version(
            WEBVIEW2_CLIENT_KEY,
            "pv",
            requirement,
            PrerequisiteArchitecture::X86,
            evidence(),
        )?;
        if machine != DetectionResult::Missing || scope == zup_core::SelectedScope::Machine {
            return Ok(machine);
        }
        return match windows_registry::CURRENT_USER.open(WEBVIEW2_CLIENT_KEY) {
            Ok(key) => detect_key_version(&key, WEBVIEW2_CLIENT_KEY, "pv", requirement, evidence()),
            Err(error) if registry_not_found(&error) => Ok(DetectionResult::Missing),
            Err(error) => Err(BootstrapError::Requirement(format!(
                "Evergreen user inventory: {error}"
            ))),
        };
    }
    if id == runtime_requirements::DOTNET_DESKTOP {
        return detect_dotnet(
            operation.target,
            "Microsoft.WindowsDesktop.App",
            requirement,
            evidence(),
        );
    }
    if id == runtime_requirements::DOTNET_RUNTIME {
        return detect_dotnet(
            operation.target,
            "Microsoft.NETCore.App",
            requirement,
            evidence(),
        );
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
    let architectures: &[PrerequisiteArchitecture] =
        if architecture == PrerequisiteArchitecture::Any {
            &[
                PrerequisiteArchitecture::X86,
                PrerequisiteArchitecture::X64,
                PrerequisiteArchitecture::Arm64,
            ]
        } else {
            std::slice::from_ref(&architecture)
        };
    let mut versions = Vec::new();
    for architecture in architectures {
        if !supported_architectures()
            .iter()
            .any(|arch| architecture_name(*arch) == architecture_name(*architecture))
        {
            continue;
        }
        let path = format!(
            "{DOTNET_INSTALLED_VERSIONS_KEY}\\{}\\sharedfx\\{runtime}",
            architecture_name(*architecture)
        );
        // .NET's installer writes all architecture inventories in the 32-bit view.
        let Some(key) = open_machine_key(&path, PrerequisiteArchitecture::X86)? else {
            continue;
        };
        for name in registry_value_names(&key)? {
            if key.get_u32(&name).map_err(|error| {
                BootstrapError::Requirement(format!(".NET inventory marker: {error}"))
            })? != 1
            {
                return Err(BootstrapError::Requirement(format!(
                    "invalid .NET inventory marker {name}"
                )));
            }
            versions.push(Version::parse(&name).map_err(|error| {
                BootstrapError::Requirement(format!(
                    "invalid .NET inventory {path}: {name}: {error}"
                ))
            })?);
        }
    }
    select_version(versions, requirement, evidence)
}

fn registry_value_names(key: &Key) -> Result<Vec<String>, BootstrapError> {
    use windows::Win32::System::Registry::{HKEY, RegEnumValueW};
    let mut names = Vec::new();
    for index in 0..65536 {
        let mut buffer = [0u16; 16384];
        let mut length = buffer.len() as u32;
        let status = unsafe {
            RegEnumValueW(
                HKEY(key.as_raw()),
                index,
                Some(windows::core::PWSTR(buffer.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        match status.0 {
            259 => return Ok(names),
            0 => names.push(
                String::from_utf16(&buffer[..length as usize]).map_err(|error| {
                    BootstrapError::Requirement(format!("invalid inventory name: {error}"))
                })?,
            ),
            code => {
                return Err(BootstrapError::Requirement(format!(
                    "enumerate native inventory: Win32 {code}"
                )));
            }
        }
    }
    Err(BootstrapError::Requirement(
        "native inventory exceeds limit".into(),
    ))
}

fn select_version(
    mut versions: Vec<Version>,
    requirement: Option<&VersionReq>,
    evidence: String,
) -> Result<DetectionResult, BootstrapError> {
    versions.sort();
    if let Some(version) = versions
        .iter()
        .rev()
        .find(|version| requirement.is_none_or(|req| req.matches(version)))
    {
        return Ok(DetectionResult::Satisfied {
            version: Some(version.clone()),
            evidence,
        });
    }
    Ok(match versions.pop() {
        Some(found) => DetectionResult::Incompatible {
            found,
            required: requirement.expect("no unconstrained mismatch").clone(),
        },
        None => DetectionResult::Missing,
    })
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
    detect_key_version(&key, path, value_name, requirement, evidence)
}

fn detect_key_version(
    key: &Key,
    path: &str,
    value_name: &str,
    requirement: Option<&VersionReq>,
    evidence: String,
) -> Result<DetectionResult, BootstrapError> {
    let value = read_version_value(key, value_name)?.ok_or_else(|| {
        BootstrapError::Requirement(format!(
            "missing native inventory value {path}: {value_name}"
        ))
    })?;
    let version = parse_runtime_version(&value)
        .ok_or_else(|| BootstrapError::Requirement(format!("invalid native version: {value}")))?;
    if path == WEBVIEW2_CLIENT_KEY && version.major == 0 && version.minor == 0 && version.patch == 0
    {
        return Ok(DetectionResult::Missing);
    }
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
    architecture: PrerequisiteArchitecture,
    scope: zup_core::SelectedScope,
) -> Result<DetectionResult, BootstrapError> {
    let product_code = package.id.as_str();
    let requirement = package.version.as_ref();
    if !product_code.is_ascii()
        || product_code.len() != 38
        || !product_code.starts_with('{')
        || !product_code.ends_with('}')
        || uuid::Uuid::parse_str(&product_code[1..37]).is_err()
    {
        return Err(BootstrapError::Requirement(
            "MSI ProductCode must be a braced GUID".into(),
        ));
    }
    use windows::Win32::System::ApplicationInstallationAndServicing::*;
    let mut versions = Vec::new();
    let mut wrong_architecture = false;
    let contexts = if scope == zup_core::SelectedScope::Machine {
        &[MSIINSTALLCONTEXT_MACHINE][..]
    } else {
        &[
            MSIINSTALLCONTEXT_MACHINE,
            MSIINSTALLCONTEXT_USERMANAGED,
            MSIINSTALLCONTEXT_USERUNMANAGED,
        ][..]
    };
    for &context in contexts {
        let Some(state) = product_property(product_code, context, "State")? else {
            continue;
        };
        match state.as_str() {
            "1" => continue,
            "5" => {}
            _ => {
                return Err(BootstrapError::Requirement(format!(
                    "invalid MSI product state {state}"
                )));
            }
        }
        if architecture != PrerequisiteArchitecture::Any {
            let path = product_property(product_code, context, "LocalPackage")?
                .ok_or_else(|| BootstrapError::Requirement("MSI product disappeared".into()))?;
            if msi_architecture(&path)? != architecture_name(architecture) {
                wrong_architecture = true;
                continue;
            }
        }
        let text = product_property(product_code, context, "VersionString")?.ok_or_else(|| {
            BootstrapError::Requirement("MSI product disappeared during inventory query".into())
        })?;
        versions.push(
            parse_runtime_version(&text).ok_or_else(|| {
                BootstrapError::Requirement(format!("invalid MSI version {text}"))
            })?,
        );
    }
    if versions.is_empty() && wrong_architecture {
        return Err(BootstrapError::Requirement(
            "MSI product is installed for a different architecture".into(),
        ));
    }
    select_version(
        versions,
        requirement,
        format!("MSI {product_code}, {scope:?} prerequisite context"),
    )
}

fn msi_architecture(path: &str) -> Result<&'static str, BootstrapError> {
    use windows::Win32::System::ApplicationInstallationAndServicing::*;
    let path = wide(path);
    let mut handle = MSIHANDLE::default();
    let status = unsafe {
        MsiGetSummaryInformationW(
            MSIHANDLE::default(),
            windows::core::PCWSTR(path.as_ptr()),
            0,
            &mut handle,
        )
    };
    if status != 0 {
        return Err(BootstrapError::Requirement(format!(
            "MSI cached package summary: Win32 {status}"
        )));
    }
    struct Summary(MSIHANDLE);
    impl Drop for Summary {
        fn drop(&mut self) {
            unsafe {
                MsiCloseHandle(self.0);
            }
        }
    }
    let handle = Summary(handle);
    let template = read_msi_string(|buffer, length| unsafe {
        MsiSummaryInfoGetPropertyW(
            handle.0,
            7,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            None,
            buffer,
            Some(length),
        )
    })?
    .ok_or_else(|| BootstrapError::Requirement("missing MSI template".into()))?;
    match template.split(';').next() {
        Some("Intel") => Ok("x86"),
        Some("x64") => Ok("x64"),
        Some("Arm64") => Ok("arm64"),
        _ => Err(BootstrapError::Requirement(format!(
            "unsupported MSI architecture {template}"
        ))),
    }
}

fn product_property(
    product_code: &str,
    context: windows::Win32::System::ApplicationInstallationAndServicing::MSIINSTALLCONTEXT,
    property: &str,
) -> Result<Option<String>, BootstrapError> {
    let product = wide(&product_code.to_ascii_uppercase());
    let attribute = wide(property);
    read_msi_string(|buffer, length| unsafe {
        windows::Win32::System::ApplicationInstallationAndServicing::MsiGetProductInfoExW(
            windows::core::PCWSTR(product.as_ptr()),
            windows::core::PCWSTR::null(),
            context,
            windows::core::PCWSTR(attribute.as_ptr()),
            buffer,
            Some(length),
        )
    })
}

fn read_msi_string(
    mut query: impl FnMut(Option<windows::core::PWSTR>, *mut u32) -> u32,
) -> Result<Option<String>, BootstrapError> {
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_UNKNOWN_PRODUCT};
    const SUCCESS: u32 = ERROR_SUCCESS.0;
    const MORE_DATA: u32 = ERROR_MORE_DATA.0;
    const UNKNOWN_PRODUCT: u32 = ERROR_UNKNOWN_PRODUCT.0;
    let mut length = 0;
    let status = query(None, &mut length);
    match status {
        SUCCESS | MORE_DATA => {}
        UNKNOWN_PRODUCT => return Ok(None),
        code => {
            return Err(BootstrapError::Requirement(format!(
                "MSI query: Win32 {code}"
            )));
        }
    }
    for _ in 0..4 {
        if length > 32767 {
            return Err(BootstrapError::Requirement(
                "MSI string exceeds limit".into(),
            ));
        }
        let mut buffer = vec![0u16; length as usize + 1];
        length = buffer.len() as u32;
        match query(Some(windows::core::PWSTR(buffer.as_mut_ptr())), &mut length) {
            MORE_DATA => continue,
            SUCCESS if (length as usize) < buffer.len() && buffer[length as usize] == 0 => {
                return String::from_utf16(&buffer[..length as usize])
                    .map(Some)
                    .map_err(|error| {
                        BootstrapError::Requirement(format!("invalid MSI UTF-16: {error}"))
                    });
            }
            code => {
                return Err(BootstrapError::Requirement(format!(
                    "MSI string read: Win32 {code}"
                )));
            }
        }
    }
    Err(BootstrapError::Requirement(
        "MSI string changed repeatedly".into(),
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

#[cfg(test)]
#[path = "prerequisites_tests.rs"]
mod native_tests;

impl PrerequisiteProvider for WindowsPrerequisiteProvider {
    fn execute(&self, request: &ProviderRequest) -> Result<ProviderOutcome, BootstrapError> {
        execute_held(request, |_| {})
    }
}

fn execute_held(
    request: &ProviderRequest,
    before_launch: impl FnOnce(&VerifiedArtifact),
) -> Result<ProviderOutcome, BootstrapError> {
    let artifact = VerifiedArtifact::acquire(request)
        .map_err(|error| BootstrapError::ProviderPreflight(error.to_string()))?;
    before_launch(&artifact);
    let launched = (|| {
        let launch = plan_launch(request, &artifact)?;
        artifact
            .source
            .validate()
            .map_err(|error| BootstrapError::ProviderPreflight(error.to_string()))?;
        Command::new(&launch.program)
            .args(&launch.arguments)
            .current_dir(
                artifact
                    .source
                    .path()
                    .parent()
                    .expect("native installer has a parent"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                BootstrapError::ProviderPreflight(format!("start prerequisite: {error}"))
            })
    })();
    let mut child = match launched {
        Ok(child) => child,
        Err(error) => {
            if let Err(cleanup) = artifact.source.cleanup() {
                tracing::warn!(%cleanup, "prerequisite preflight cleanup failed");
            }
            return Err(error);
        }
    };
    let status = child
        .wait()
        .map_err(|error| BootstrapError::Provider(format!("wait for prerequisite: {error}")))?;
    let code = status.code().ok_or_else(|| {
        BootstrapError::Provider("prerequisite terminated without an exit code".into())
    })?;
    if request.success_exit_codes.contains(&code) {
        if let Err(error) = artifact.source.cleanup() {
            tracing::warn!(%error, "prerequisite execution source retained");
        }
        Ok(ProviderOutcome::Succeeded)
    } else if code == 1641 || request.reboot_exit_codes.contains(&code) {
        Ok(ProviderOutcome::RebootRequired { exit_code: code })
    } else {
        Err(BootstrapError::Provider(format!(
            "prerequisite exited with code {code}"
        )))
    }
}

struct Launch<'a> {
    program: PathBuf,
    arguments: Vec<String>,
    _artifact: &'a VerifiedArtifact,
}

fn plan_launch<'a>(
    request: &ProviderRequest,
    artifact: &'a VerifiedArtifact,
) -> Result<Launch<'a>, BootstrapError> {
    let launch = match artifact.format {
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
                artifact.source.path().display().to_string(),
                "/qn".to_owned(),
            ];
            arguments.extend(request.arguments.iter().cloned());
            arguments.push("/norestart".to_owned());
            Launch {
                program: msiexec,
                arguments,
                _artifact: artifact,
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
                program: artifact.source.path().to_owned(),
                arguments,
                _artifact: artifact,
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

struct VerifiedArtifact {
    source: crate::filesystem::execution::ExecutionSource,
    format: ArtifactFormat,
}

impl VerifiedArtifact {
    fn acquire(request: &ProviderRequest) -> Result<Self, BootstrapError> {
        let expected_digest = request
            .package
            .digest()
            .ok_or_else(|| BootstrapError::ProviderPreflight("missing artifact digest".into()))?;
        let expected_size = request.package.size();
        let filename = request
            .package
            .filename()
            .ok_or_else(|| BootstrapError::ProviderPreflight("missing artifact filename".into()))?;
        let mut source =
            crate::filesystem::execution::ExecutionSource::acquire(&request.executable, filename)
                .map_err(|error| BootstrapError::Provider(format!("artifact open: {error}")))?;
        let verified = (|| {
            let (size, digest) = zup_core::hash_reader(
                source
                    .file()
                    .take(zup_core::MAX_PREREQUISITE_PACKAGE_BYTES + 1),
            )
            .map_err(|error| BootstrapError::Provider(format!("artifact hash: {error}")))?;
            if size > zup_core::MAX_PREREQUISITE_PACKAGE_BYTES
                || digest != expected_digest
                || expected_size.is_some_and(|expected| expected != size)
            {
                return Err(BootstrapError::Provider(
                    "artifact digest or size changed before execution".into(),
                ));
            }
            classify_artifact(source.file())
        })();
        match verified {
            Ok(format) => Ok(Self { source, format }),
            Err(error) => {
                if let Err(cleanup) = source.cleanup() {
                    tracing::warn!(%cleanup, "rejected prerequisite cleanup failed");
                }
                Err(error)
            }
        }
    }
}

fn classify_artifact(file: &mut std::fs::File) -> Result<ArtifactFormat, BootstrapError> {
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| BootstrapError::ProviderPreflight(error.to_string()))?;
    let mut magic = [0u8; COMPOUND_FILE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) if magic == COMPOUND_FILE_MAGIC => Ok(ArtifactFormat::WindowsInstaller),
        Ok(()) if &magic[..2] == b"MZ" => Ok(ArtifactFormat::Executable),
        _ => Err(BootstrapError::ProviderPreflight(
            "unsupported installer format".into(),
        )),
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
        Type::String => String::try_from(value).map(Some).map_err(|error| {
            BootstrapError::Requirement(format!("invalid registry version: {error}"))
        }),
        _ => Err(BootstrapError::Requirement(
            "native version is not a string".into(),
        )),
    }
}

fn supported_architectures() -> Vec<PrerequisiteArchitecture> {
    use windows::Win32::System::SystemInformation::*;
    let mut info = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut info);
    }
    let native = unsafe { info.Anonymous.Anonymous.wProcessorArchitecture };
    match native {
        PROCESSOR_ARCHITECTURE_AMD64 => {
            vec![PrerequisiteArchitecture::X86, PrerequisiteArchitecture::X64]
        }
        PROCESSOR_ARCHITECTURE_ARM64 => vec![
            PrerequisiteArchitecture::X86,
            PrerequisiteArchitecture::X64,
            PrerequisiteArchitecture::Arm64,
        ],
        PROCESSOR_ARCHITECTURE_INTEL => vec![PrerequisiteArchitecture::X86],
        _ => vec![],
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
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    if let Ok(version) = Version::parse(value) {
        return Some(version);
    }
    let parts = value.split('.').collect::<Vec<_>>();
    if !(2..=4).contains(&parts.len())
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let mut version = Version::new(
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts.get(2).unwrap_or(&"0").parse().ok()?,
    );
    if let Some(revision) = parts.get(3) {
        let revision: u64 = revision.parse().ok()?;
        version.build = semver::BuildMetadata::new(&revision.to_string()).ok()?;
    }
    Some(version)
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
        let code = unsafe { windows::Win32::Foundation::GetLastError() }.0;
        return match code {
            2 | 3 | 1812..=1815 => Ok(None),
            _ => Err(BootstrapError::Requirement(format!(
                "file version size query: Win32 {code}"
            ))),
        };
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
        return Err(BootstrapError::Requirement(
            "invalid native file version record".into(),
        ));
    }
    let info = unsafe { &*info.cast::<VsFixedFileInfo>() };
    if info.signature != 0xFEEF_04BD {
        return Err(BootstrapError::Requirement(
            "invalid native file version signature".into(),
        ));
    }
    let major = (info.file_version_ms >> 16) as u64;
    let minor = (info.file_version_ms & 0xffff) as u64;
    let build = (info.file_version_ls >> 16) as u64;
    Ok(parse_runtime_version(&format!(
        "{major}.{minor}.{build}.{}",
        info.file_version_ls & 0xffff
    )))
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

    fn remote_package(filename: &str) -> zup_core::PrerequisitePackage {
        zup_core::PrerequisitePackage::Remote {
            url: format!("https://cdn.example.test/{filename}"),
            sha256: digest(filename.as_bytes()),
            size: Some(filename.len() as u64),
            filename: filename.into(),
        }
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
            package: remote_package("runtime.exe"),
            success_exit_codes: vec![0],
            reboot_exit_codes: vec![1641, 3010],
        }
    }

    fn verified_request(path: PathBuf, bytes: &[u8]) -> ProviderRequest {
        let filename = path.file_name().unwrap().to_str().unwrap().to_owned();
        let mut request = provider_request(path, runtime_requirement("fixture.runtime"));
        request.package = zup_core::PrerequisitePackage::Remote {
            url: format!("https://example.test/{filename}"),
            filename,
            sha256: digest(bytes),
            size: Some(bytes.len() as u64),
        };
        request
    }

    fn authenticate(request: &ProviderRequest) -> Result<VerifiedArtifact, BootstrapError> {
        VerifiedArtifact::acquire(request)
    }

    #[test]
    fn artifact_format_comes_from_verified_content() {
        let package = compound_file_bytes();
        let (_root, path) = artifact(&package, "runtime.msi");
        assert_eq!(
            authenticate(&verified_request(path.clone(), &package))
                .unwrap()
                .format,
            ArtifactFormat::WindowsInstaller
        );
        let (_root, tampered) = artifact(b"not the declared package", "runtime.msi");
        assert!(authenticate(&verified_request(tampered, &package)).is_err());
        let mut request = verified_request(path, &package);
        if let zup_core::PrerequisitePackage::Remote { size, .. } = &mut request.package {
            *size = Some(1);
        }
        assert!(VerifiedArtifact::acquire(&request).is_err());

        let executable = [PE_MAGIC.as_slice(), &[0u8; 64]].concat();
        let (_root, path) = artifact(&executable, "runtime.exe");
        assert_eq!(
            authenticate(&verified_request(path, &executable))
                .unwrap()
                .format,
            ArtifactFormat::Executable
        );
    }

    #[test]
    fn a_launch_plan_carries_the_silent_contract_of_its_artifact() {
        let package = compound_file_bytes();
        let (_root, installer) = artifact(&package, "runtime.msi");
        let mut request = verified_request(installer.clone(), &package);
        request.arguments = vec!["ALLUSERS=1".to_owned()];
        let artifact = authenticate(&request).unwrap();
        let launch = plan_launch(&request, &artifact).unwrap();
        assert_eq!(
            launch.program,
            system_directory().unwrap().join("msiexec.exe")
        );
        assert_eq!(
            launch.arguments,
            [
                "/i".to_owned(),
                artifact.source.path().display().to_string(),
                "/qn".to_owned(),
                "ALLUSERS=1".to_owned(),
                "/norestart".to_owned(),
            ]
        );

        drop(artifact);
        let executable_bytes = [b"MZ".as_slice(), &[0u8; 64]].concat();
        let (_root, executable) = self::artifact(&executable_bytes, "runtime.exe");
        for (requirement, arguments, expected) in [
            (
                runtime_requirement(runtime_requirements::VISUAL_CPP_V14),
                Vec::new(),
                vec!["/install", "/quiet", "/norestart"],
            ),
            (
                runtime_requirement(runtime_requirements::WEBVIEW2_EVERGREEN),
                Vec::new(),
                vec!["/silent", "/install", "/norestart"],
            ),
            (
                runtime_requirement(runtime_requirements::DOTNET_DESKTOP),
                vec!["/norestart".to_owned()],
                vec!["/install", "/quiet", "/norestart"],
            ),
            (
                PrerequisiteRequirement::InstalledPackage(InstalledPackage {
                    id: InstalledPackageId::new(package_requirements::WEBVIEW2_BOOTSTRAPPER)
                        .unwrap(),
                    version: None,
                }),
                Vec::new(),
                vec!["/norestart"],
            ),
        ] {
            let mut request = verified_request(executable.clone(), &executable_bytes);
            request.requirement = requirement;
            request.arguments = arguments;
            let artifact = authenticate(&request).unwrap();
            let launch = plan_launch(&request, &artifact).unwrap();
            assert_eq!(
                launch.program,
                artifact.source.path(),
                "an executable runs itself"
            );
            assert_eq!(launch.arguments, expected);
        }
    }

    #[test]
    fn detection_separates_an_absent_requirement_from_an_unsupported_one() {
        let unknown_runtime = BootstrapOperation {
            id: PrerequisiteId::new("runtime").unwrap(),
            name: "Runtime".into(),
            target: PrerequisiteArchitecture::Current,
            requirement: runtime_requirement("windows.vc.v99"),
            package: remote_package("vc.exe"),
            installer: zup_core::PrerequisiteInstaller::default(),
        };
        assert!(matches!(
            WindowsPrerequisiteDetector::new(zup_core::SelectedScope::User)
                .satisfy(&unknown_runtime),
            Err(BootstrapError::Requirement(_))
        ));

        let absent_package = BootstrapOperation {
            id: PrerequisiteId::new("runtime").unwrap(),
            name: "Runtime".into(),
            target: PrerequisiteArchitecture::Current,
            requirement: PrerequisiteRequirement::InstalledPackage(InstalledPackage {
                id: InstalledPackageId::new("{00000000-0000-0000-0000-000000000000}").unwrap(),
                version: None,
            }),
            package: remote_package("pkg.msi"),
            installer: zup_core::PrerequisiteInstaller::default(),
        };
        assert_eq!(
            WindowsPrerequisiteDetector::new(zup_core::SelectedScope::User)
                .satisfy(&absent_package)
                .unwrap(),
            DetectionResult::Missing
        );
    }
}
