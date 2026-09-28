//! The portable boundary.
//!
//! A portable crate is platform-neutral by construction. It may not depend on a
//! Windows crate, name a Windows API, branch on `cfg(windows)` in production
//! code, reintroduce an identifier that presents a Windows concept as part of
//! the portable model, or spell a Windows concept inside a string literal.
//! Windows is allowed in the Windows adapter, in the product frontends and
//! binaries, in tests and documentation, and inside target-lexicon identifiers
//! such as `TargetOperatingSystem::Windows`.
//!
//! The concept vocabulary is closed: registry, COM class ids, the service
//! control manager, elevation, named pipes, installer packages and the
//! runtimes they install, PE resources, known folders, and the Windows payload
//! and receipt registration. Blanking string content hid a leak through a
//! literal, so literals are scanned separately from code, with escapes
//! resolved, and matched case-insensitively.
//!
//! Comments are still blanked: prose may explain what a portable crate refuses
//! to know, and a doc comment is not production behaviour. This crate is the
//! one exemption, because its source is the vocabulary. The portable
//! vocabulary that happens to share a word with a Windows concept stays legal
//! by construction rather than by allowlist: `InstallLocation::Desktop` is an
//! install location, `Service` is a portable model type, `Payload` and
//! `Receipt` are portable transaction and bundle terms, and `Privilege` names
//! an actor rather than an elevation mechanism.
//!
//! # Two vocabularies, one boundary
//!
//! [`matrix::Vocabulary`] decides whether the concept tables apply at all. A
//! `Domain` package is held to all of them. A `FileFormat` package is held to
//! every *structural* rule - no Windows dependency, no `cfg(windows)`, no
//! `std::os::windows`, no Win32 namespace - and to neither concept table,
//! because a crate whose domain is a Windows file format must name that format
//! to be about it at all. `RCDATA` in `zup-pe` is the format's resource type,
//! not a Windows concept leaking into a portable model.
//!
//! The relaxation is declared once, in the matrix every other command reads, so
//! a package does not gain it by a line added next to the code it silences.
//!
//! Adding a rule means adding a [`Rule`] variant, its summary, a token table,
//! and one [`check_source`] arm. Findings are ordered by matrix order, then
//! package, then path, then line, so two runs over one workspace are equal.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::matrix;
use crate::workspace::{self, Member};

/// Crate names a portable package may not depend on. A name matches when it is
/// the prefix itself or the prefix followed by `-`.
pub const FORBIDDEN_DEPENDENCY_PREFIXES: &[&str] = &["windows", "winapi", "zup-windows"];

/// Manifest tables that can declare a dependency.
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// The package that owns the rule tables in this module. Its source *is* the
/// banned vocabulary, so it is the one portable package exempt from the source
/// scan; its manifest is still checked.
pub const ENFORCER: &str = "zup-xtask";

/// Platform predicates a portable package may not branch on. `cfg(not(windows))`
/// is a platform branch too: it is where a portable crate stops being portable.
pub const WINDOWS_CFG_TOKENS: &[&str] = &[
    "cfg(windows)",
    "cfg!(windows)",
    "cfg(not(windows))",
    "cfg(target_os = \"windows\")",
    "cfg(target_family = \"windows\")",
];

/// Platform-specific `std` modules a portable package may not import.
pub const OS_WINDOWS_TOKENS: &[&str] = &["std::os::windows"];

/// Windows API namespaces a portable package may not name.
pub const WINDOWS_API_TOKENS: &[&str] =
    &["windows::Win32", "Win32::", "winapi::", "windows_bindgen"];

/// Identifiers that encode a Windows concept and belong to the adapter.
///
/// Matching is case-sensitive, so a token never fires on a portable
/// identifier that merely uses a different casing of the same letters.
pub const BANNED_IDENTIFIERS: &[&str] = &[
    // Registry, which also covers `RegistryHive`, `RegistryKey`, and
    // `RegistryValue` as prefixes.
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
    // COM class identifiers.
    "ProgId",
    "CLSID",
    // The service control manager.
    "ServiceControlManager",
    "OpenSCManager",
    "StartService",
    "StopService",
    "ChangeServiceConfig",
    "DeleteService",
    "SERVICE_WIN32",
    // Elevation.
    "IsUserAnAdmin",
    "ShellExecute",
    "RequestedExecutionLevel",
    // Named pipes.
    "CreateNamedPipe",
    "ConnectNamedPipe",
    "WaitNamedPipe",
    "PipeServer",
    "PipeClient",
    // Installer packages and the runtimes they install.
    "MsiProduct",
    "MsiInstaller",
    "WindowsInstaller",
    "msiexec",
    "VisualCpp",
    "WebView2",
    "VCRedist",
    // PE resources.
    "RCDATA",
    "IconGroup",
    "VersionInfoResource",
    // Known folders.
    "KnownFolder",
    "FOLDERID",
    "SHGetKnownFolderPath",
    "SHGetFolderPath",
    "CSIDL",
    // The Windows payload and receipt registration.
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
    // Rejected before the concept list existed.
    "PrerequisiteDetector",
    "PrerequisiteInstallerKind",
    "Shortcut",
    "ManagedResource",
];

/// Windows concept tokens a portable package may not spell in a string
/// literal, matched case-insensitively because the same concept is written
/// `HKEY_LOCAL_MACHINE`, `hkey_local_machine`, and `HkeyLocalMachine`.
///
/// Every token is compared against literal content with escapes already
/// resolved, so a token may contain a single backslash and still match the
/// escaped `\\` spelling in the source. Tokens are chosen to be distinctive: a
/// token that also occurs inside an ordinary word is a defect in this table,
/// not a portable concept.
pub const BANNED_LITERALS: &[&str] = &[
    // Registry.
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
    // COM class identifiers.
    "progid",
    "clsid",
    // The service control manager.
    "servicecontrolmanager",
    "openscmanager",
    "service_win32",
    "sc.exe",
    "sc create",
    "sc delete",
    // Elevation.
    "runas",
    "shellexecute",
    "isuseranadmin",
    "requestedexecutionlevel",
    "requireadministrator",
    "highestavailable",
    "asinvoker",
    // Named pipes.
    "\\\\.\\pipe\\",
    "named_pipe",
    "pipe_access",
    // Installer packages and the runtimes they install.
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
    // PE resources.
    "rcdata",
    "pe_resource",
    "pe32",
    "vs_version_info",
    "icon_group",
    "rt_icon",
    // Known folders.
    "folderid",
    "knownfolder",
    "shgetknownfolder",
    "shgetfolderpath",
    "csidl",
    "programdata",
    "programfiles",
    "localappdata",
    // The Windows payload and receipt registration.
    "uninstallstring",
    "uninstall_string",
    "uninstallregistry",
    "appsfeaturesstate",
    "appsfeaturesvalue",
    "arpentry",
    "windowsreceipt",
    "bundlereceipt",
    ".zup-payload-overlays",
];

/// One boundary rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// The matrices and the workspace members must correspond exactly.
    MatrixMembership,
    /// A portable crate depends on a Windows-only crate.
    ForbiddenDependency,
    /// A portable manifest selects a Windows-only platform.
    WindowsTargetScope,
    /// Portable production code imports a platform-specific `std` module.
    OsWindowsImport,
    /// Portable production code names a Windows API namespace.
    WindowsApiNamespace,
    /// Portable production code branches on the build host.
    WindowsCfgBranch,
    /// Portable production code reintroduces a Windows-specific identifier.
    BannedIdentifier,
    /// Portable production code spells a Windows concept in a string literal.
    BannedLiteral,
}

impl Rule {
    /// Stable identifier used in reports.
    pub const fn id(self) -> &'static str {
        match self {
            Self::MatrixMembership => "matrix-membership",
            Self::ForbiddenDependency => "forbidden-dependency",
            Self::WindowsTargetScope => "windows-target-scope",
            Self::OsWindowsImport => "os-windows-import",
            Self::WindowsApiNamespace => "windows-api-namespace",
            Self::WindowsCfgBranch => "windows-cfg-branch",
            Self::BannedIdentifier => "banned-identifier",
            Self::BannedLiteral => "banned-literal",
        }
    }

    /// What the rule protects.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::MatrixMembership => "the package matrices and the workspace must agree",
            Self::ForbiddenDependency => "portable crate depends on a Windows-only crate",
            Self::WindowsTargetScope => "portable manifest selects a Windows-only platform",
            Self::OsWindowsImport => {
                "portable production code imports a platform-specific std module"
            }
            Self::WindowsApiNamespace => "portable production code names a Windows API namespace",
            Self::WindowsCfgBranch => "portable production code branches on the build host",
            Self::BannedIdentifier => {
                "portable production code reintroduces a Windows-specific identifier"
            }
            Self::BannedLiteral => {
                "portable production code spells a Windows concept in a string literal"
            }
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// One finding. `path` is workspace-relative with `/` separators, `line` is
/// 1-based and absent for manifest and matrix findings.
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

/// Check every portable package, and that the matrices match the workspace.
pub fn check_workspace(root: &Path) -> Result<Vec<Violation>, String> {
    let members = workspace::members(root)?;
    let mut violations = coverage_violations(&members);
    violations.extend(check_members(root, &members)?);
    Ok(violations)
}

/// Boundary rule findings for every portable member. Matrix membership is
/// reported separately by [`coverage_violations`].
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
        // A file-format crate is held to every structural rule and none of the
        // vocabulary ones. Decided by the matrix rather than by an allowlist here,
        // so the classification is the one the rest of the tooling already reads.
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

/// Workspace members without a matrix, and matrix packages without a member.
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

/// Production Rust sources of one package: `src/**/*.rs` plus `build.rs`.
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
        if is_windows_target(target) {
            violations.push(Violation {
                rule: Rule::WindowsTargetScope,
                package: member.name.clone(),
                path: location.clone(),
                line: None,
                detail: format!("[{label}] is a Windows-only platform table"),
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
    // The vocabulary tables are what a file-format crate is exempt from; every
    // other table applies to it unchanged.
    let mut structural = vec![
        (Rule::WindowsCfgBranch, WINDOWS_CFG_TOKENS),
        (Rule::OsWindowsImport, OS_WINDOWS_TOKENS),
        (Rule::WindowsApiNamespace, WINDOWS_API_TOKENS),
    ];
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

fn is_forbidden_dependency(name: &str) -> bool {
    FORBIDDEN_DEPENDENCY_PREFIXES.iter().any(|prefix| {
        name == *prefix
            || name
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('-'))
    })
}

fn is_windows_target(target: &str) -> bool {
    let compact = target
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    compact == "cfg(windows)"
        || compact == "cfg(target_os=\"windows\")"
        || compact == "cfg(target_family=\"windows\")"
}

/// One scrubbed source line. `code` holds everything but comment and string
/// content; `literals` holds only string content, with escapes resolved, so a
/// concept spelled in a literal is matched in its decoded form.
struct Line {
    code: String,
    literals: String,
}

/// Splits a line into code and literal content, blanking comments, so prose
/// cannot satisfy or trip a rule while data still can.
#[derive(Default)]
struct Scrubber {
    block: bool,
}

impl Scrubber {
    fn scrub(&mut self, line: &str) -> Line {
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
        }
    }
}

/// Whether the quote at `index` opens a raw literal, as in `r"..."`. A
/// byte-string prefix is not raw, and a hash-delimited raw string is read to
/// its first quote.
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

/// The character an escape sequence denotes, or `None` for an escape this
/// gate does not decode. An undecoded escape keeps its backslash, so a
/// concept token is never matched against a mangled literal.
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

/// Tracks `#[cfg(test)]` blocks, whose contents are tests rather than
/// production code.
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
    use matrix::{Host, MATRICES, Vocabulary};

    fn rules(violations: &[Violation]) -> Vec<Rule> {
        violations.iter().map(|violation| violation.rule).collect()
    }

    fn member(name: &str) -> Member {
        Member {
            name: name.to_owned(),
            directory: format!("crates/{name}"),
        }
    }

    /// Write `source` to a scratch file and scan it under one vocabulary.
    fn scan(vocabulary: Vocabulary, source: &str) -> Vec<Rule> {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("source.rs");
        fs::write(&path, source).expect("write");
        rules(&check_source(&member("probe"), "probe.rs", &path, vocabulary).expect("scan"))
    }

    /// The relaxation is to the concept tables and to nothing else. Every
    /// structural rule still fires for a file-format crate, which is what makes
    /// the tier a boundary rather than an exemption. A tier that quietly dropped a
    /// structural rule would be indistinguishable from no tier at all. The `cfg`
    /// branch is the one that matters: a file-format crate is the most plausible
    /// place to smuggle one in.
    #[test]
    fn a_file_format_crate_is_still_held_to_every_structural_rule() {
        for (source, expected) in [
            ("#[cfg(windows)]\npub fn f() {}\n", Rule::WindowsCfgBranch),
            (
                "use std::os::windows::ffi::OsStrExt;\n",
                Rule::OsWindowsImport,
            ),
            (
                "use windows::Win32::System::SystemInformation;\n",
                Rule::WindowsApiNamespace,
            ),
        ] {
            assert_eq!(
                scan(Vocabulary::FileFormat, source),
                vec![expected],
                "a file-format crate is not exempt from {expected}"
            );
        }
    }

    /// A file-format crate's own vocabulary is the format's. `RCDATA` is what a
    /// PE resource type is called in the PE specification, so a PE parser that
    /// cannot write it is not a PE parser.
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

    /// The classification comes from the matrix, so a package cannot acquire the
    /// file-format relaxation by anything written next to the code it silences.
    /// An unclassified package gets the strict rules, not none - the default is
    /// the whole point, so it is what this pins.
    #[test]
    fn the_vocabulary_is_the_matrixs_and_an_unclaimed_package_gets_the_strict_one() {
        assert_eq!(matrix::vocabulary_of("zup-pe"), Vocabulary::FileFormat);
        assert_eq!(
            matrix::vocabulary_of("no-such-crate"),
            Vocabulary::Domain,
            "an unclassified package is held to the strict rules"
        );
    }

    /// A file-format crate is portable by definition. One that needed a Windows
    /// build host would be a host adapter wearing a file format's name, and the
    /// gate would be checking the wrong thing.
    /// A file-format crate is portable by definition, so it must be built on
    /// every host rather than verified only where a Windows toolchain exists.
    /// Otherwise a cross-platform matrix member is quietly never compiled.
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
        }
    }

    /// The scrubber is what keeps prose from satisfying or tripping a rule, and
    /// a file-format crate is held to the structural rules exactly as a domain
    /// crate is, so its comments must be blanked the same way.
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
