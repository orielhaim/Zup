use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::matrix;
use crate::workspace::{self, Member};

pub const FORBIDDEN_DEPENDENCY_PREFIXES: &[&str] = &[
    "windows",
    "winapi",
    "windows-bindgen",
    "nix",
    "rustix",
    "zbus",
    "libc",
];

const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

pub const ENFORCER: &str = "zup-xtask";

pub const PLATFORM_CFG_TOKENS: &[&str] = &[
    "cfg(windows)",
    "cfg!(windows)",
    "cfg(not(windows))",
    "cfg(unix)",
    "cfg!(unix)",
    "cfg(not(unix))",
];

pub const PLATFORM_CFG_VALUES: &[&str] = &[
    "windows", "linux", "unix", "macos", "ios", "android", "wasi",
];

const UNIX_CFG_TOKENS: &[&str] = &["cfg(unix)", "cfg!(unix)", "cfg(not(unix))"];

const UNIX_CFG_VALUES: &[&str] = &["linux", "unix"];

pub const OS_PLATFORM_TOKENS: &[&str] = &["std::os::windows", "std::os::linux", "std::os::unix"];

const WINDOWS_OS_TOKENS: &[&str] = &["std::os::windows", "std::os::linux"];

pub const NATIVE_API_TOKENS: &[&str] = &[
    "windows::Win32",
    "Win32::",
    "winapi::",
    "windows_bindgen",
    "rustix::",
    "zbus::",
];

pub const BANNED_IDENTIFIERS: &[&str] = &[
    "Registry",
    "RegOpenKey",
    "RegQueryValue",
    "RegSetValue",
    "RegDeleteKey",
    "RegCloseKey",
    "HKEY_LOCAL_MACHINE",
    "HKEY_CURRENT_USER",
    "HKEY_CLASSES_ROOT",
    "HKEY_USERS",
    "ProgId",
    "CLSID",
    "ServiceControlManager",
    "OpenSCManager",
    "StartService",
    "StopService",
    "ChangeServiceConfig",
    "DeleteService",
    "SERVICE_WIN32",
    "IsUserAnAdmin",
    "ShellExecute",
    "RequestedExecutionLevel",
    "CreateNamedPipe",
    "ConnectNamedPipe",
    "WaitNamedPipe",
    "PipeServer",
    "PipeClient",
    "MsiProduct",
    "MsiInstaller",
    "WindowsInstaller",
    "msiexec",
    "VisualCpp",
    "WebView2",
    "VCRedist",
    "RCDATA",
    "IconGroup",
    "VersionInfoResource",
    "KnownFolder",
    "FOLDERID",
    "SHGetKnownFolderPath",
    "SHGetFolderPath",
    "CSIDL",
    "UninstallEntry",
    "UninstallRegistry",
    "AppsFeaturesState",
    "AppsFeaturesOperation",
    "AppsFeaturesValue",
    "ArpEntry",
    "PayloadOverlayError",
    "PayloadOverlayDirectory",
    "WindowsReceipt",
    "BundleReceipt",
    "PrerequisiteDetector",
    "PrerequisiteInstallerKind",
    "Shortcut",
    "ManagedResource",
    "Systemd",
    "SystemdUnit",
    "Dbus",
    "DBus",
    "DesktopEntry",
    "DesktopFile",
    "MimeAssociation",
    "Hicolor",
    "XdgStateDir",
    "XdgConfigDir",
    "XdgDataDir",
    "Polkit",
    "Pkexec",
    "AppImage",
    "Landlock",
    "Seccomp",
    "Pidfd",
];

pub const BANNED_LITERALS: &[&str] = &[
    "registry",
    "hkey_local_machine",
    "hkey_current_user",
    "hkey_classes_root",
    "hkey_users",
    "regopenkey",
    "regqueryvalue",
    "regsetvalue",
    "regdeletekey",
    "regclosekey",
    "localmachine\\",
    "currentuser\\",
    "software\\microsoft",
    "shell\\open\\command",
    "defaulticon",
    "progid",
    "clsid",
    "servicecontrolmanager",
    "openscmanager",
    "service_win32",
    "sc.exe",
    "sc create",
    "sc delete",
    "runas",
    "shellexecute",
    "isuseranadmin",
    "requestedexecutionlevel",
    "requireadministrator",
    "highestavailable",
    "asinvoker",
    "\\\\.\\pipe\\",
    "named_pipe",
    "pipe_access",
    "msi",
    "msiexec",
    "windowsinstaller",
    "msi_product",
    "webview2",
    "evergreen",
    "msedgewebview2",
    "vcredist",
    "visual c++",
    "vc++",
    "visualcpp",
    "msvcp",
    "vcruntime",
    "rcdata",
    "pe_resource",
    "pe32",
    "vs_version_info",
    "icon_group",
    "rt_icon",
    "folderid",
    "knownfolder",
    "shgetknownfolder",
    "shgetfolderpath",
    "csidl",
    "programdata",
    "programfiles",
    "localappdata",
    "uninstallstring",
    "uninstall_string",
    "uninstallregistry",
    "appsfeaturesstate",
    "appsfeaturesvalue",
    "arpentry",
    "windowsreceipt",
    "bundlereceipt",
    ".zup-payload-overlays",
    "systemd",
    "dbus",
    "org.freedesktop",
    "hicolor",
    "xdg-state",
    "xdg_config",
    "xdg-data",
    "polkit",
    "pkexec",
    "appimage",
    "landlock",
    "seccomp",
    "pidfd_open",
    "/etc/systemd",
    "/usr/share/applications",
    "/var/lib",
];

pub const SCENARIO_FILES: &[(&str, &str)] = &[
    ("zup-preview", "crates/zup-preview/src/catalog.rs"),
    ("zup-preview", "crates/zup-preview/src/machine.rs"),
];

pub fn is_scenario(package: &str, location: &str) -> bool {
    SCENARIO_FILES
        .iter()
        .any(|(owner, path)| *owner == package && *path == location)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    MatrixMembership,
    ForbiddenDependency,
    PlatformTargetScope,
    OsPlatformImport,
    NativeApiNamespace,
    PlatformCfgBranch,
    BannedIdentifier,
    BannedLiteral,
}

impl Rule {
    pub const fn id(self) -> &'static str {
        match self {
            Self::MatrixMembership => "matrix-membership",
            Self::ForbiddenDependency => "forbidden-dependency",
            Self::PlatformTargetScope => "platform-target-scope",
            Self::OsPlatformImport => "os-platform-import",
            Self::NativeApiNamespace => "native-api-namespace",
            Self::PlatformCfgBranch => "platform-cfg-branch",
            Self::BannedIdentifier => "banned-identifier",
            Self::BannedLiteral => "banned-literal",
        }
    }

    pub const fn summary(self) -> &'static str {
        match self {
            Self::MatrixMembership => "the package matrices and the workspace must agree",
            Self::ForbiddenDependency => "portable crate depends on a native backend",
            Self::PlatformTargetScope => "portable manifest selects a single-platform table",
            Self::OsPlatformImport => {
                "portable production code imports a platform-specific std module"
            }
            Self::NativeApiNamespace => "portable production code names a native API namespace",
            Self::PlatformCfgBranch => "portable production code branches on a platform",
            Self::BannedIdentifier => {
                "portable production code reintroduces a native-backend identifier"
            }
            Self::BannedLiteral => {
                "portable production code spells a native concept in a string literal"
            }
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: Rule,
    pub package: String,
    pub path: String,
    pub line: Option<usize>,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let location = match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        };
        write!(
            f,
            "{} ({location}) [{}] {}: {}",
            self.package,
            self.rule,
            self.rule.summary(),
            self.detail
        )
    }
}

pub fn check_workspace(root: &Path) -> Result<Vec<Violation>, String> {
    let members = workspace::members(root)?;
    let mut violations = coverage_violations(&members);
    violations.extend(check_members(root, &members)?);
    Ok(violations)
}

pub fn check(root: &Path) -> Result<Vec<Violation>, String> {
    check_members(root, &workspace::members(root)?)
}

fn check_members(root: &Path, members: &[Member]) -> Result<Vec<Violation>, String> {
    let mut violations = Vec::new();
    for package in matrix::portable() {
        let Some(member) = members.iter().find(|member| member.name == package) else {
            continue;
        };
        let directory = root.join(&member.directory);
        violations.extend(check_manifest(member, &directory)?);
        if member.name == ENFORCER {
            continue;
        }
        let vocabulary = matrix::vocabulary_of(package);
        for source in production_sources(&directory)? {
            violations.extend(check_source(
                member,
                &format!(
                    "{}/{}",
                    member.directory,
                    workspace::relative(&directory, &source)
                ),
                &source,
                vocabulary,
            )?);
        }
    }
    Ok(violations)
}

pub fn coverage_violations(members: &[Member]) -> Vec<Violation> {
    let classified = matrix::all();
    let mut violations: Vec<Violation> = matrix::duplicated_packages()
        .into_iter()
        .map(|package| {
            membership(
                "Cargo.toml",
                package,
                format!("matrix package `{package}` is in more than one matrix"),
            )
        })
        .collect();
    for member in members {
        if !classified.contains(&member.name.as_str()) {
            violations.push(membership(
                &member.directory,
                &member.name,
                format!("unclassified workspace member `{}`", member.name),
            ));
        }
    }
    for package in &classified {
        if !members.iter().any(|member| member.name == **package) {
            violations.push(membership(
                "Cargo.toml",
                package,
                format!("matrix package `{package}` is not a workspace member"),
            ));
        }
    }
    violations
}

fn membership(path: &str, package: &str, detail: String) -> Violation {
    Violation {
        rule: Rule::MatrixMembership,
        package: package.to_owned(),
        path: path.to_owned(),
        line: None,
        detail,
    }
}

fn production_sources(package: &Path) -> Result<Vec<PathBuf>, String> {
    let mut sources = BTreeSet::new();
    let build = package.join("build.rs");
    if build.is_file() {
        sources.insert(build);
    }
    collect_rust_files(&package.join("src"), &mut sources)?;
    Ok(sources.into_iter().collect())
}

fn collect_rust_files(directory: &Path, sources: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    if !directory.is_dir() {
        return Ok(());
    }
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, sources)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.insert(path);
        }
    }
    Ok(())
}

fn check_manifest(member: &Member, package: &Path) -> Result<Vec<Violation>, String> {
    let path = package.join("Cargo.toml");
    let manifest = workspace::read_manifest(&path)?;
    let location = format!("{}/Cargo.toml", member.directory);
    let mut violations = Vec::new();
    for table in DEPENDENCY_TABLES {
        scan_dependencies(
            member,
            &location,
            table,
            manifest.get(table),
            &mut violations,
        );
    }
    let Some(targets) = manifest.get("target").and_then(Value::as_table) else {
        return Ok(violations);
    };
    for (target, tables) in targets {
        let label = format!("target.'{target}'");
        if let Some(platform) = single_platform_target(target) {
            violations.push(Violation {
                rule: Rule::PlatformTargetScope,
                package: member.name.clone(),
                path: location.clone(),
                line: None,
                detail: format!(
                    "[{label}] is a {}-only platform table",
                    platform.crate_name()
                ),
            });
        }
        let Some(tables) = tables.as_table() else {
            continue;
        };
        for table in DEPENDENCY_TABLES {
            scan_dependencies(
                member,
                &location,
                &format!("{label}.{table}"),
                tables.get(*table),
                &mut violations,
            );
        }
    }
    Ok(violations)
}

fn scan_dependencies(
    member: &Member,
    location: &str,
    label: &str,
    table: Option<&Value>,
    violations: &mut Vec<Violation>,
) {
    let Some(table) = table.and_then(Value::as_table) else {
        return;
    };
    let mut names = table.keys().map(String::as_str).collect::<Vec<_>>();
    names.sort_unstable();
    for name in names {
        if !is_forbidden_dependency(name) {
            continue;
        }
        violations.push(Violation {
            rule: Rule::ForbiddenDependency,
            package: member.name.clone(),
            path: location.to_owned(),
            line: None,
            detail: format!("[{label}] declares {name}"),
        });
    }
}

fn check_source(
    member: &Member,
    location: &str,
    path: &Path,
    vocabulary: matrix::Vocabulary,
) -> Result<Vec<Violation>, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let delegates_lifecycle = matrix::delegates_platform_lifecycle(&member.name);
    let writes_mode_bits = matrix::writes_posix_permissions(&member.name);
    let mut structural = vec![
        (
            Rule::OsPlatformImport,
            if writes_mode_bits {
                WINDOWS_OS_TOKENS
            } else {
                OS_PLATFORM_TOKENS
            },
        ),
        (Rule::NativeApiNamespace, NATIVE_API_TOKENS),
    ];
    let (allowed_cfg, allowed_cfg_values) = match (delegates_lifecycle, writes_mode_bits) {
        (true, _) => (PLATFORM_CFG_TOKENS, PLATFORM_CFG_VALUES),
        (false, true) => (UNIX_CFG_TOKENS, UNIX_CFG_VALUES),
        (false, false) => (&[][..], &[][..]),
    };
    if vocabulary == matrix::Vocabulary::Domain {
        structural.push((Rule::BannedIdentifier, BANNED_IDENTIFIERS));
    }
    let mut scrubber = Scrubber::default();
    let mut modules = TestModules::default();
    let mut violations = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = scrubber.scrub(line);
        modules.observe(&line.code);
        if modules.inside() {
            continue;
        }
        let number = index + 1;
        let branches = platform_cfg_matches(&line, allowed_cfg, allowed_cfg_values);
        if !branches.is_empty() {
            violations.push(Violation {
                rule: Rule::PlatformCfgBranch,
                package: member.name.clone(),
                path: location.to_owned(),
                line: Some(number),
                detail: branches.join(", "),
            });
        }
        for (rule, tokens) in &structural {
            let matched = tokens
                .iter()
                .filter(|token| line.code.contains(**token))
                .copied()
                .collect::<Vec<_>>();
            if matched.is_empty() {
                continue;
            }
            violations.push(Violation {
                rule: *rule,
                package: member.name.clone(),
                path: location.to_owned(),
                line: Some(number),
                detail: matched.join(", "),
            });
        }
        if vocabulary != matrix::Vocabulary::Domain {
            continue;
        }
        if is_scenario(&member.name, location) {
            continue;
        }
        let literals = line.literals.to_ascii_lowercase();
        let matched = BANNED_LITERALS
            .iter()
            .filter(|token| literals.contains(**token))
            .copied()
            .collect::<Vec<_>>();
        if matched.is_empty() {
            continue;
        }
        violations.push(Violation {
            rule: Rule::BannedLiteral,
            package: member.name.clone(),
            path: location.to_owned(),
            line: Some(number),
            detail: matched.join(", "),
        });
    }
    Ok(violations)
}

fn platform_cfg_matches(
    line: &Line,
    allowed_tokens: &[&str],
    allowed_values: &[&str],
) -> Vec<String> {
    let mut out: Vec<String> = PLATFORM_CFG_TOKENS
        .iter()
        .filter(|token| line.code.contains(**token) && !allowed_tokens.contains(*token))
        .map(|token| (*token).to_owned())
        .collect();
    let code = line.code.as_str();
    let raw: Vec<char> = line.raw.chars().collect();
    let mut search = 0;
    while let Some(found) = code[search..].find("cfg") {
        let start = search + found;
        search = start + 3;
        if start > 0
            && code[..start]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_alphanumeric() || character == '_')
        {
            continue;
        }
        let Some((argument, bang)) = cfg_argument(&raw[start + 3..]) else {
            continue;
        };
        for value in PLATFORM_CFG_VALUES {
            if allowed_values.contains(value) {
                continue;
            }
            let named = argument == format!("target_os=\"{value}\"")
                || argument == format!("target_family=\"{value}\"");
            let negated = argument.strip_prefix("not(").is_some_and(|inner| {
                inner == format!("target_os=\"{value}\"")
                    || inner == format!("target_family=\"{value}\"")
            });
            if !named && !negated {
                continue;
            }
            let spelled = format!("cfg{}({argument})", if bang { "!" } else { "" });
            if !out.contains(&spelled) {
                out.push(spelled);
            }
        }
    }
    out
}

fn cfg_argument(characters: &[char]) -> Option<(String, bool)> {
    let text: String = characters.iter().collect();
    let (after, bang) = text
        .strip_prefix('!')
        .map(|after| (after, true))
        .or_else(|| text.strip_prefix('(').map(|after| (after, false)))?;
    let mut depth = 0usize;
    for (index, character) in after.char_indices() {
        match character {
            '(' => depth += 1,
            ')' if depth == 0 => {
                return Some((after[..index].replace([' ', '\t'], ""), bang));
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn is_forbidden_dependency(name: &str) -> bool {
    matrix::backends()
        .iter()
        .chain(FORBIDDEN_DEPENDENCY_PREFIXES)
        .any(|prefix| name_matches_prefix(name, prefix))
}

fn name_matches_prefix(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn single_platform_target(target: &str) -> Option<matrix::Platform> {
    let compact = target
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    match compact.as_str() {
        "cfg(windows)" | "cfg(target_os=\"windows\")" | "cfg(target_family=\"windows\")" => {
            Some(matrix::Platform::Windows)
        }
        "cfg(target_os=\"linux\")" | "cfg(target_family=\"unix\")" => Some(matrix::Platform::Linux),
        _ => None,
    }
}

struct Line {
    code: String,
    literals: String,
    raw: String,
}

#[derive(Default)]
struct Scrubber {
    block: bool,
}

impl Scrubber {
    fn scrub(&mut self, line: &str) -> Line {
        let raw = line.to_owned();
        let characters = line.chars().collect::<Vec<_>>();
        let mut code = vec![' '; characters.len()];
        let mut literals: Vec<char> = Vec::new();
        let mut index = 0;
        while index < characters.len() {
            let character = characters[index];
            if self.block {
                if character == '*' && characters.get(index + 1) == Some(&'*') {
                    self.block = false;
                    index += 2;
                } else {
                    if character == '\n' {
                        code[index] = '\n';
                    }
                    index += 1;
                }
                continue;
            }
            match character {
                '/' if characters.get(index + 1) == Some(&'/') => break,
                '/' if characters.get(index + 1) == Some(&'*') => {
                    self.block = true;
                    index += 2;
                }
                '"' => {
                    code[index] = '"';
                    let raw = is_raw_prefix(&code, index);
                    index += 1;
                    while index < characters.len() {
                        match characters[index] {
                            '\\' if !raw => {
                                index += 1;
                                match characters.get(index).copied().and_then(resolve_escape) {
                                    Some(decoded) => {
                                        literals.push(decoded);
                                        index += 1;
                                    }
                                    None => literals.push('\\'),
                                }
                            }
                            '"' | '\n' => {
                                if characters[index] == '"' {
                                    code[index] = '"';
                                }
                                index += 1;
                                break;
                            }
                            _ => {
                                literals.push(characters[index]);
                                index += 1;
                            }
                        }
                    }
                }
                _ => {
                    code[index] = character;
                    index += 1;
                }
            }
        }
        Line {
            code: code.into_iter().collect(),
            literals: literals.into_iter().collect(),
            raw,
        }
    }
}

fn is_raw_prefix(code: &[char], index: usize) -> bool {
    let mut cursor = index;
    while cursor > 0 && code[cursor - 1] == 'r' {
        cursor -= 1;
    }
    if cursor == index {
        return false;
    }
    match cursor.checked_sub(1).and_then(|before| code.get(before)) {
        Some(previous) => !previous.is_alphanumeric() && *previous != '_',
        None => true,
    }
}

fn resolve_escape(escape: char) -> Option<char> {
    match escape {
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        '0' => Some('\0'),
        '\\' | '\'' | '"' => Some(escape),
        _ => None,
    }
}

#[derive(Default)]
struct TestModules {
    depth: usize,
    anchor: Option<usize>,
    inside: bool,
}

impl TestModules {
    fn observe(&mut self, code: &str) {
        if self.anchor.is_none() && code.contains("#[cfg(test)]") {
            self.anchor = Some(self.depth);
        }
        self.depth += code.matches('{').count();
        self.depth = self.depth.saturating_sub(code.matches('}').count());
        if let Some(anchor) = self.anchor {
            if self.inside {
                if self.depth <= anchor {
                    self.inside = false;
                    self.anchor = None;
                }
            } else if self.depth > anchor {
                self.inside = true;
            }
        }
    }

    fn inside(&self) -> bool {
        self.inside
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix::{Host, Kind, MATRICES, Vocabulary};

    fn rules(violations: &[Violation]) -> Vec<Rule> {
        violations.iter().map(|violation| violation.rule).collect()
    }

    fn member(name: &str) -> Member {
        Member {
            name: name.to_owned(),
            directory: format!("crates/{name}"),
        }
    }

    fn scan_as(package: &str, vocabulary: Vocabulary, source: &str) -> Vec<Rule> {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("source.rs");
        fs::write(&path, source).expect("write");
        rules(&check_source(&member(package), "probe.rs", &path, vocabulary).expect("scan"))
    }

    fn scan(vocabulary: Vocabulary, source: &str) -> Vec<Rule> {
        scan_as("zup-core", vocabulary, source)
    }

    #[test]
    fn a_file_format_crate_is_still_held_to_every_structural_rule() {
        for (source, expected) in [
            ("#[cfg(windows)]\npub fn f() {}\n", Rule::PlatformCfgBranch),
            (
                "#[cfg(target_os = \"linux\")]\npub fn f() {}\n",
                Rule::PlatformCfgBranch,
            ),
            (
                "use std::os::windows::ffi::OsStrExt;\n",
                Rule::OsPlatformImport,
            ),
            (
                "use std::os::unix::fs::PermissionsExt;\n",
                Rule::OsPlatformImport,
            ),
            (
                "use windows::Win32::System::SystemInformation;\n",
                Rule::NativeApiNamespace,
            ),
            ("use rustix::fs::stat;\n", Rule::NativeApiNamespace),
        ] {
            assert_eq!(
                scan(Vocabulary::FileFormat, source),
                vec![expected],
                "a file-format crate is not exempt from {expected}"
            );
        }
    }

    #[test]
    fn a_portable_crate_may_not_acquire_either_backends_vocabulary() {
        for (source, expected) in [
            ("#[cfg(windows)]\npub fn f() {}\n", Rule::PlatformCfgBranch),
            (
                "#[cfg(target_os = \"linux\")]\npub fn f() {}\n",
                Rule::PlatformCfgBranch,
            ),
            ("#[cfg(unix)]\npub fn f() {}\n", Rule::PlatformCfgBranch),
            (
                "use std::os::windows::ffi::OsStrExt;\n",
                Rule::OsPlatformImport,
            ),
            (
                "use std::os::unix::fs::PermissionsExt;\n",
                Rule::OsPlatformImport,
            ),
            (
                "use windows::Win32::System::SystemInformation;\n",
                Rule::NativeApiNamespace,
            ),
            ("use rustix::fs::stat;\n", Rule::NativeApiNamespace),
            (
                "pub const UNIT: &str = \"/etc/systemd/system/acme.service\";\n",
                Rule::BannedLiteral,
            ),
            (
                "pub const BUS: &str = \"org.freedesktop.Notify\";\n",
                Rule::BannedLiteral,
            ),
            (
                "fn f() { let _ = SystemdUnit::new(); }\n",
                Rule::BannedIdentifier,
            ),
        ] {
            assert_eq!(
                scan(Vocabulary::Domain, source),
                vec![expected],
                "{source:?} must be refused in a portable crate"
            );
        }
    }

    #[test]
    fn a_file_format_crate_may_name_the_format() {
        assert!(
            scan(
                Vocabulary::FileFormat,
                "pub const RESOURCE_TYPE_RCDATA: u16 = 10;\n"
            )
            .is_empty(),
            "the format's vocabulary is not a leak"
        );
        assert_eq!(
            scan(
                Vocabulary::Domain,
                "pub const RESOURCE_TYPE_RCDATA: u16 = 10;\n"
            ),
            vec![Rule::BannedIdentifier],
            "a domain crate has no such excuse"
        );
    }

    #[test]
    fn the_vocabulary_is_the_matrixs_and_an_unclaimed_package_gets_the_strict_one() {
        assert_eq!(matrix::vocabulary_of("zup-pe"), Vocabulary::FileFormat);
        assert_eq!(
            matrix::vocabulary_of("zup-binary"),
            Vocabulary::FileFormat,
            "a crate that reads PE/COFF, ELF and Mach-O is a file-format crate"
        );
        assert_eq!(
            matrix::vocabulary_of("no-such-crate"),
            Vocabulary::Domain,
            "an unclassified package is held to the strict rules"
        );
    }

    #[test]
    fn a_lifecycle_delegating_package_may_name_the_two_wrappers() {
        assert_eq!(
            scan_as(
                "zup-preset-host",
                Vocabulary::Domain,
                "#[cfg(windows)]\nfn f() {}\n#[cfg(unix)]\nfn g() {}\n"
            ),
            Vec::new(),
            "the branch that selects process-wrap's per-platform tree mechanism is the one thing \
             it is exempt from"
        );
        assert_eq!(
            scan_as(
                "zup-preset-host",
                Vocabulary::Domain,
                "use std::os::windows::ffi::OsStrExt;\n"
            ),
            vec![Rule::OsPlatformImport],
            "and a platform std import is still a violation: the exemption is for the wrapper \
             names, not for reaching around them"
        );
        assert_eq!(
            scan_as(
                "zup-preset-host",
                Vocabulary::Domain,
                "use windows::Win32::System::SystemInformation;\n"
            ),
            vec![Rule::NativeApiNamespace],
            "as is naming a Win32 namespace directly"
        );
        assert_eq!(
            scan_as(
                "zup-preset-host",
                Vocabulary::Domain,
                "pub const KEY: &str = \"HKEY_LOCAL_MACHINE\";\n"
            ),
            vec![Rule::BannedLiteral],
            "and the concept tables apply unchanged"
        );
    }

    #[test]
    fn the_permission_relaxation_buys_one_import_and_nothing_else() {
        assert_eq!(
            scan_as(
                "zup-preview",
                Vocabulary::Domain,
                "#[cfg(unix)]\nfn f() {}\nuse std::os::unix::fs::PermissionsExt;\n"
            ),
            Vec::new(),
            "a staged preset has to be runnable, and std has no portable way to say so"
        );
        assert_eq!(
            scan_as(
                "zup-preview",
                Vocabulary::Domain,
                "#[cfg(windows)]\nfn f() {}\n"
            ),
            vec![Rule::PlatformCfgBranch],
            "but choosing between two backends' permission models is a backend decision"
        );
        assert_eq!(
            scan_as(
                "zup-preview",
                Vocabulary::Domain,
                "use std::os::windows::ffi::OsStrExt;\n"
            ),
            vec![Rule::OsPlatformImport],
            "as is the Windows half of the platform std surface"
        );
        assert_eq!(
            scan_as("zup-preview", Vocabulary::Domain, "use rustix::fs::stat;\n"),
            vec![Rule::NativeApiNamespace],
            "and it grants no native API surface either"
        );
        assert_eq!(
            scan_as("zup-core", Vocabulary::Domain, "#[cfg(unix)]\nfn f() {}\n"),
            vec![Rule::PlatformCfgBranch],
            "the relaxation is declared per package, so an ordinary portable crate still gets \
             the strict answer"
        );
    }

    #[test]
    fn a_backend_package_is_a_forbidden_dependency_for_a_portable_crate() {
        for backend in matrix::backends() {
            assert!(
                is_forbidden_dependency(backend),
                "{backend} is a backend, so a portable crate may not depend on it"
            );
            assert!(
                is_forbidden_dependency(&format!("{backend}-sys")),
                "and the prefix rule covers a family, not one spelling"
            );
        }
        assert!(is_forbidden_dependency("windows"));
        assert!(is_forbidden_dependency("windows-link"));
        assert!(!is_forbidden_dependency("zup-core"));
        assert!(!is_forbidden_dependency("windowsy"));
    }

    #[test]
    fn a_single_platform_target_table_is_recognised_for_every_platform() {
        assert_eq!(
            single_platform_target("cfg(windows)"),
            Some(matrix::Platform::Windows)
        );
        assert_eq!(
            single_platform_target("cfg( target_os = \"windows\" )"),
            Some(matrix::Platform::Windows)
        );
        assert_eq!(
            single_platform_target("cfg(target_os = \"linux\")"),
            Some(matrix::Platform::Linux)
        );
        assert_eq!(
            single_platform_target("cfg(target_family=\"unix\")"),
            Some(matrix::Platform::Linux)
        );
        assert_eq!(single_platform_target("cfg(unix)"), None);
        assert_eq!(single_platform_target("x86_64-pc-windows-msvc"), None);
    }

    #[test]
    fn the_relaxations_are_the_matrixs_and_are_claimed_by_someone() {
        assert!(matrix::delegates_platform_lifecycle("zup-preset-host"));
        assert!(
            matrix::delegates_platform_lifecycle("zup-preset-dev"),
            "the crate that supervises a compiler owns the same lifecycle"
        );
        assert!(
            !matrix::delegates_platform_lifecycle("no-such-crate"),
            "an unclassified package is still held to the strict rules"
        );
        assert!(
            !matrix::delegates_platform_lifecycle("zup-core"),
            "and a package with no children to own does not get it"
        );
        for package in matrix::PORTABLE_PLATFORM_DELEGATING
            .iter()
            .chain(matrix::PORTABLE_POSIX_PERMISSIONS)
        {
            assert!(
                matrix::is_portable(package),
                "{package} holds a relaxation, so it is portable: the two classifications are \
                 not alternatives"
            );
        }
    }

    #[test]
    fn each_backend_is_verified_on_its_own_host() {
        for platform in matrix::Platform::ALL {
            let crate_name = platform.crate_name();
            let entry = MATRICES
                .iter()
                .find(|entry| entry.kind == Kind::Backend(*platform))
                .unwrap_or_else(|| panic!("`{crate_name}` is claimed by a backend matrix"));
            assert_eq!(entry.packages, &[crate_name]);
            assert_eq!(entry.host, platform.host(), "{crate_name}");
            assert!(
                !matrix::is_portable(crate_name),
                "a backend is never verified as portable"
            );
            assert_eq!(
                matrix::sibling_backends(*platform).len(),
                matrix::Platform::ALL.len() - 1,
                "{crate_name} has every other backend as a sibling and not itself"
            );
        }
        assert!(
            !matrix::is_portable("zup-windows"),
            "and that holds for the backend that existed first too"
        );
        assert!(
            matrix::is_composition("zup-installer"),
            "composition tooling is verified on a native host but is not portable"
        );
    }

    #[test]
    fn a_file_format_crate_is_verified_on_every_host() {
        let file_formats: Vec<_> = MATRICES
            .iter()
            .filter(|entry| entry.vocabulary == Vocabulary::FileFormat)
            .collect();
        assert!(
            !file_formats.is_empty(),
            "the tier must be claimed by someone"
        );
        for entry in file_formats {
            assert_eq!(
                entry.host,
                Host::Any,
                "`{}` is a file format, so it builds everywhere",
                entry.name
            );
            assert!(
                entry.kind.is_portable(),
                "and it is portable, not composition: the two are not alternatives"
            );
        }
    }

    #[test]
    fn a_comment_is_not_production_code() {
        assert!(
            scan(
                Vocabulary::FileFormat,
                "// #[cfg(windows)] is what we refuse\n"
            )
            .is_empty()
        );
        assert!(
            scan(Vocabulary::FileFormat, "/* std::os::windows::ffi */\n").is_empty(),
            "a block comment is closed before the next line is read"
        );
    }
}
