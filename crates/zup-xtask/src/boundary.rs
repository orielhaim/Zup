//! The portable boundary.
//!
//! A portable crate is platform-neutral by construction. It may not depend on a
//! native backend, name a native API, branch on a platform predicate in
//! production code, reintroduce an identifier that presents a native concept as
//! part of the portable model, or spell a native concept inside a string
//! literal.
//!
//! The rule is no longer about Windows specifically. It is: **a portable crate
//! must not acquire the vocabulary of any native backend.** Windows and Linux
//! are siblings here, so a rule written against only one of them would be a rule
//! with a hole shaped like every other platform. Each table below is written
//! once and applied to all of them, and adding a platform adds it to
//! [`matrix::Platform`] rather than adding an exception here.
//!
//! One structural rule has a narrow, declared relaxation: a package that owns a
//! child process may branch to choose between the two process-tree mechanisms
//! `process-wrap` provides, because it has no portable spelling for "the tree is
//! mine". See [`matrix::PORTABLE_PLATFORM_DELEGATING`]. A second, narrower one
//! lets a package write a POSIX mode bit, because `std` declines to abstract
//! permissions. See [`matrix::PORTABLE_POSIX_PERMISSIONS`]. Every other rule
//! still applies to both.
//!
//! Native vocabulary is allowed inside a native backend, in composition tooling
//! that reaches a backend deliberately, in tests and documentation, and inside
//! target-lexicon identifiers such as `TargetOperatingSystem::Windows`.
//!
//! The concept vocabulary is closed: the mechanisms one Windows backend owns
//! (registry, COM class ids, the service control manager, elevation, named
//! pipes, installer packages, PE resources, known folders) and the ones a Linux
//! backend owns (systemd, D-Bus, desktop entries, XDG base directories,
//! privilege escalation). Blanking string content hid a leak through a literal,
//! so literals are scanned separately from code, with escapes resolved, and
//! matched case-insensitively.
//!
//! Comments are still blanked: prose may explain what a portable crate refuses
//! to know, and a doc comment is not production behaviour. This crate is the
//! one exemption, because its source is the vocabulary. The portable
//! vocabulary that happens to share a word with a native concept stays legal
//! by construction rather than by allowlist: `InstallLocation::Desktop` is an
//! install location, `Service` is a portable model type, `Payload` and
//! `Receipt` are portable transaction and bundle terms, and `Privilege` names
//! an actor rather than an elevation mechanism.
//!
//! # Two vocabularies, one boundary
//!
//! [`matrix::Vocabulary`] decides whether the concept tables apply at all. A
//! `Domain` package is held to all of them. A `FileFormat` package is held to
//! every *structural* rule - no backend dependency, no platform `cfg`, no
//! platform `std` import, no native API namespace - and to neither concept
//! table, because a crate whose domain is a platform file format must name that
//! format to be about it at all. `RCDATA` in `zup-pe` is the format's resource
//! type, not a Windows concept leaking into a portable model.
//!
//! The relaxation is declared once, in the matrix every other command reads, so
//! a package does not gain it by a line added next to the code it silences.
//!
//! Adding a rule means adding a [`Rule`] variant, its summary, a token table,
//! and one [`check_source`] arm.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::matrix;
use crate::workspace::{self, Member};

/// Crate names a portable package may not depend on, beyond the native backends.
///
/// The backends themselves come from [`matrix::backends`], so this table holds
/// only third-party families whose presence in a portable graph means the graph
/// reached a platform. A name matches when it is the prefix itself or the
/// prefix followed by `-`.
pub const FORBIDDEN_DEPENDENCY_PREFIXES: &[&str] = &[
    "windows",
    "winapi",
    "windows-bindgen",
    // The Linux mechanism layer is reached through one or two of these rather
    // than through a dozen platform-specific names, so the families are what a
    // portable crate has to stay out of: a raw syscall surface, a Linux session
    // bus, and the C library both of the others are built on.
    "nix",
    "rustix",
    "zbus",
    "libc",
];

/// Manifest tables that can declare a dependency.
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// The package that owns the rule tables in this module. Its source *is* the
/// banned vocabulary, so it is the one portable package exempt from the source
/// scan; its manifest is still checked.
pub const ENFORCER: &str = "zup-xtask";

/// Bare platform predicates a portable package may not branch on.
///
/// These carry no string literal, so they survive comment-blanking and can be
/// matched as ordinary code tokens. Every spelling of each is here, including the
/// negated and `cfg!` forms, because each is a place a portable crate stops being
/// portable.
pub const PLATFORM_CFG_TOKENS: &[&str] = &[
    "cfg(windows)",
    "cfg!(windows)",
    "cfg(not(windows))",
    "cfg(unix)",
    "cfg!(unix)",
    "cfg(not(unix))",
];

/// The operating-system and target-family values a portable crate may not branch
/// on, matched inside a `cfg` attribute rather than as free text.
///
/// The scrubber blanks string content so a literal cannot satisfy a rule, and
/// `#[cfg(target_os = "linux")]` is *entirely* a literal. These are therefore
/// read from the `cfg` attributes themselves - see [`platform_cfg_matches`] -
/// rather than from a token table, which is why the two lists above and here
/// both exist instead of one merged list that would silently never fire.
pub const PLATFORM_CFG_VALUES: &[&str] = &[
    "windows", "linux", "unix", "macos", "ios", "android", "wasi",
];

/// The Unix half of the platform branch rule, which is what the *permission*
/// relaxation buys.
///
/// A package that writes a POSIX mode bit has to say `cfg(unix)` to reach the
/// permission API, and that is the entire extent of the relaxation. `windows`
/// stays refused: choosing between a Windows permission model and a POSIX mode is
/// choosing between two platform backends, and that belongs at a native boundary.
/// The other declared relaxation - the process-tree lifecycle - is not modelled
/// here because it legitimately names *both* branches, one per `process-wrap`
/// wrapper.
const UNIX_CFG_TOKENS: &[&str] = &["cfg(unix)", "cfg!(unix)", "cfg(not(unix))"];

/// The key/value `cfg` values a package holding a Unix relaxation may still name:
/// the two spellings of "a POSIX system".
const UNIX_CFG_VALUES: &[&str] = &["linux", "unix"];

/// Platform-specific `std` modules a portable package may not import.
pub const OS_PLATFORM_TOKENS: &[&str] = &["std::os::windows", "std::os::linux", "std::os::unix"];

/// The Windows half of [`OS_PLATFORM_TOKENS`], which stays refused even of a
/// package that writes a POSIX mode bit.
const WINDOWS_OS_TOKENS: &[&str] = &["std::os::windows", "std::os::linux"];

/// Native API namespaces a portable package may not name.
///
/// `nix` is absent deliberately: the token `nix::` is a substring of `unix::`,
/// which every portable crate that follows the process-wrap delegation already
/// spells. The `nix` crate is refused where it can actually be reached - the
/// manifest, through [`FORBIDDEN_DEPENDENCY_PREFIXES`] - because a crate cannot
/// name a path it does not depend on. The two families kept below are the ones
/// whose *namespace* spelling varies while the crate name does not.
pub const NATIVE_API_TOKENS: &[&str] = &[
    "windows::Win32",
    "Win32::",
    "winapi::",
    "windows_bindgen",
    "rustix::",
    "zbus::",
];

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
    // The Linux mechanism layer: the init system, the session bus, the
    // desktop-environment registration a Linux install is expressed through, and
    // the privilege mechanisms that stand in for elevation. Each is a concept
    // exactly one platform can be right about, which is the whole reason a
    // portable model may not name one.
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
    // The Linux mechanism layer.
    //
    // `.desktop` is deliberately absent: it is the suffix of a desktop-entry
    // *file*, but it is also the tail of a reverse-DNS application identifier,
    // which every portable manifest carries. A token that fires on
    // `com.acme.desktop` is a token that would have to be allowlisted, and the
    // concept is already caught by `DesktopEntry`/`DesktopFile` on the code side
    // and by the two directory spellings below.
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

/// Source files that describe a machine rather than model one.
///
/// The portable boundary is about a package's model: it must not present a
/// Windows concept as part of what it can represent. A preview scenario is not
/// that. It is a description of a machine somebody might install onto, and
/// naming the prerequisite such a machine would be asked for is the entire point
/// of the scenario - a portable preview environment still previews the thing an
/// installer actually installs.
///
/// The exemption is declared per package and per file, and it covers the literal
/// table only: every structural rule still applies to these files, so a scenario
/// that branched on the build host or reached for a Win32 namespace would be
/// refused exactly as it would be anywhere else.
pub const SCENARIO_FILES: &[(&str, &str)] = &[
    // The named states `zup preview` offers. Each one describes a Windows
    // machine, so each one may name what that machine would be missing.
    ("zup-preview", "crates/zup-preview/src/catalog.rs"),
    // A scenario's footprint, which records where a demo installation lands.
    ("zup-preview", "crates/zup-preview/src/machine.rs"),
];

/// Whether a source file describes a machine rather than modelling one.
pub fn is_scenario(package: &str, location: &str) -> bool {
    SCENARIO_FILES
        .iter()
        .any(|(owner, path)| *owner == package && *path == location)
}
/// One boundary rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// The matrices and the workspace members must correspond exactly.
    MatrixMembership,
    /// A portable crate depends on a native backend or a platform crate.
    ForbiddenDependency,
    /// A portable manifest selects a single-platform target table.
    PlatformTargetScope,
    /// Portable production code imports a platform-specific `std` module.
    OsPlatformImport,
    /// Portable production code names a native API namespace.
    NativeApiNamespace,
    /// Portable production code branches on a platform predicate.
    PlatformCfgBranch,
    /// Portable production code reintroduces a native-backend identifier.
    BannedIdentifier,
    /// Portable production code spells a native concept in a string literal.
    BannedLiteral,
}

impl Rule {
    /// Stable identifier used in reports.
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

    /// What the rule protects.
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
    // Both relaxations are read from the matrix by package name, so a package
    // cannot acquire either by anything written beside the code it silences.
    let delegates_lifecycle = matrix::delegates_platform_lifecycle(&member.name);
    let writes_mode_bits = matrix::writes_posix_permissions(&member.name);
    // The vocabulary tables are what a file-format crate is exempt from; every
    // other table applies to it unchanged.
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
    // Which platform predicates a package may still name. Read from the matrix
    // rather than from an allowlist here, so a package cannot acquire the
    // relaxation by anything written beside the code it silences. The two
    // relaxations buy different things and are not interchangeable:
    // `process-wrap` documents one wrapper *per platform*, so a package using it
    // names both branches, while a POSIX mode bit is the whole of what the
    // permission relaxation is for, so it buys the Unix half and nothing else.
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
            // A scenario describes a machine rather than modelling one, so it is
            // allowed to name what such a machine has or is missing. The
            // structural rules above have already run on this line and are
            // unaffected: a scenario still may not branch on the build host.
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

/// The platform predicates named by the `cfg` attributes on one line.
///
/// Read from the attributes rather than matched as free text, because a
/// `#[cfg(...)]` argument is a string literal and the scrubber blanks literal
/// content - a token table for these spellings would match nothing and read as a
/// rule that is enforced. The attribute's argument is read from `code`, which
/// preserves every character of the line and blanks only literal *content*, so
/// the argument is available from it while the surrounding prose still cannot
/// satisfy the rule.
fn platform_cfg_matches(
    line: &Line,
    allowed_tokens: &[&str],
    allowed_values: &[&str],
) -> Vec<String> {
    let mut out: Vec<String> = PLATFORM_CFG_TOKENS
        .iter()
        // The bare spellings carry no literal, so they are ordinary code tokens and
        // `code` is the right place to look. A relaxed package keeps its own.
        .filter(|token| line.code.contains(**token) && !allowed_tokens.contains(*token))
        .map(|token| (*token).to_owned())
        .collect();
    // The key/value spellings are entirely string literals, which `code` blanks,
    // so the attribute is located in `code` - whose character positions match
    // `raw` - and its argument read from `raw`.
    let code = line.code.as_str();
    let raw: Vec<char> = line.raw.chars().collect();
    let mut search = 0;
    while let Some(found) = code[search..].find("cfg") {
        let start = search + found;
        search = start + 3;
        // An identifier that merely ends in these letters is not a predicate, and
        // `code` has blanked every comment, so a `cfg` that survives here is one a
        // compiler would see.
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

/// The argument of the `cfg` at the start of `characters`, with balanced
/// parentheses and whitespace removed.
///
/// `not(target_os = "windows")` closes twice, and reading the first `)` would
/// leave `not(target_os = "windows", which names nothing.
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
    // The backends come from the matrix rather than from a table here, so adding
    // a platform adds its package to this rule with no edit to the rule itself.
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

/// The platform a Cargo target table selects, when it selects exactly one.
///
/// `cfg(windows)` and `cfg(target_os = "linux")` are the same kind of
/// declaration refused for the same reason, so both are recognised here rather
/// than only the one the first backend happened to need.
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

/// One scrubbed source line.
///
/// `code` holds everything but comment and string *content*, preserving the line's
/// character positions; `literals` holds only string content, with escapes
/// resolved; and `raw` is the line as written. A rule that needs a literal's
/// value reads it from `raw` at an offset `code` established, which is how a
/// `cfg` attribute's argument stays readable without letting prose satisfy a
/// rule.
struct Line {
    code: String,
    literals: String,
    raw: String,
}

/// Splits a line into code and literal content, blanking comments, so prose
/// cannot satisfy or trip a rule while data still can.
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

    /// Write `source` to a scratch file and scan it as `package` is scanned.
    fn scan_as(package: &str, vocabulary: Vocabulary, source: &str) -> Vec<Rule> {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("source.rs");
        fs::write(&path, source).expect("write");
        rules(&check_source(&member(package), "probe.rs", &path, vocabulary).expect("scan"))
    }

    /// An ordinary portable package, holding neither declared relaxation.
    fn scan(vocabulary: Vocabulary, source: &str) -> Vec<Rule> {
        scan_as("zup-core", vocabulary, source)
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

    /// The invariant this module exists to state, with Windows as the case that
    /// was already true and Linux as the one that is newly true. The two are
    /// siblings, so a portable crate is caught for either - not caught for
    /// Windows with a Linux-shaped hole sitting beside it.
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

    /// A package that delegates process-tree lifecycle to `process-wrap` may name
    /// that crate's two wrappers, because a job object and a process group are two
    /// spellings of one guarantee rather than two behaviours.
    ///
    /// The point of the test is what else still fires. A relaxation that dropped
    /// the import and namespace rules too would be indistinguishable from not
    /// enforcing the boundary here at all, so those are pinned as well.
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

    /// The permission relaxation buys exactly one thing: the ability to reach the
    /// Unix permission API and say `cfg(unix)` to get there. It does not become a
    /// general platform exemption, which is what makes it safe to grant at all.
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

    /// The dependency rule is what stops a portable crate from reaching a
    /// backend, and it is the rule a new platform most easily breaks: a backend
    /// added to the matrix but not to the prefix table would be a portable crate
    /// one dependency away from a syscall interface.
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

    /// A manifest target table that selects one platform is refused for every
    /// platform, not only for the one that happened to exist first.
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

    /// The relaxations are declared in the matrix, so they cannot be acquired by
    /// anything written next to the code they silence, and an unclassified package
    /// gets the strict answer.
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

    /// A backend is verified only where its platform can be built, and it is
    /// verified as a backend rather than as a portable crate that happens to be
    /// large. Both halves matter: the first is what makes the matrix an
    /// instruction, the second is what makes the boundary skip it deliberately.
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
            assert!(
                entry.kind.is_portable(),
                "and it is portable, not composition: the two are not alternatives"
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
