//! Boundary fixtures.
//!
//! Every fixture is a complete workspace on disk: one member per package the
//! matrices name, plus the `crates/*` member glob the repository uses. A
//! fixture then dirties exactly one package, and the matrix-membership rule
//! stays satisfied.

use std::fs;
use std::path::Path;

use rstest::rstest;
use tempfile::TempDir;
use zup_xtask::boundary::{self, Rule};
use zup_xtask::matrix;
use zup_xtask::workspace;

/// One file of a fixture crate: `(package-relative path, contents)`.
type File<'a> = (&'a str, &'a str);

/// The minimal manifest of a package with no dependencies.
fn manifest(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.0.1\"\nedition = \"2024\"\n")
}

/// A workspace holding exactly the packages the matrices name.
fn complete_workspace() -> TempDir {
    let root = TempDir::new().expect("temp workspace");
    for package in matrix::all() {
        add_package(root.path(), package, &manifest(package), &[]);
    }
    write_root(root.path(), &["crates/*"], &[]);
    root
}

/// The same workspace with `package` replaced by `manifest` and `files`.
fn workspace_with(package: &str, manifest: &str, files: &[File]) -> TempDir {
    let root = complete_workspace();
    add_package(root.path(), package, manifest, files);
    root
}

fn add_package(root: &Path, package: &str, manifest: &str, files: &[File]) {
    let directory = root.join("crates").join(package);
    fs::create_dir_all(&directory).expect("package dir");
    fs::write(directory.join("Cargo.toml"), manifest).expect("package manifest");
    for (relative, contents) in files.iter().copied() {
        let path = directory.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("nested dir");
        }
        fs::write(path, contents).expect("fixture file");
    }
}

fn write_root(root: &Path, members: &[&str], exclude: &[&str]) {
    let list = |entries: &[&str]| {
        entries
            .iter()
            .map(|entry| format!("\"{entry}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut manifest = format!(
        "[workspace]\nresolver = \"3\"\nmembers = [{}]\n",
        list(members)
    );
    if !exclude.is_empty() {
        manifest.push_str(&format!("exclude = [{}]\n", list(exclude)));
    }
    fs::write(root.join("Cargo.toml"), manifest).expect("root manifest");
}

fn rules(root: &Path) -> Vec<Rule> {
    let mut rules: Vec<Rule> = boundary::check_workspace(root)
        .expect("check runs")
        .into_iter()
        .map(|violation| violation.rule)
        .collect();
    rules.sort();
    rules.dedup();
    rules
}

fn findings(root: &Path) -> Vec<String> {
    boundary::check_workspace(root)
        .expect("check runs")
        .into_iter()
        .map(|violation| violation.to_string())
        .collect()
}

/// The negative control. Every other fixture in this file dirties exactly one
/// thing and expects a finding; without a workspace that is clean by construction,
/// a rule that stopped firing would look identical to a rule that never worked.
#[test]
fn a_clean_portable_workspace_has_no_findings() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[
            (
                "src/lib.rs",
                "use zup_core::TargetOperatingSystem;\n\
                 pub fn platform() -> TargetOperatingSystem {\n    \
                 TargetOperatingSystem::Windows\n}\n",
            ),
            (
                "build.rs",
                "// A portable build script has nothing to generate.\nfn main() {}\n",
            ),
        ],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

/// The fixture workspace is built from the matrices, so this checks that the
/// fixture builder did what it claims. The real coverage gate is
/// `every_member_of_this_workspace_is_classified`, which runs against this
/// repository rather than a fixture built from the same table.
#[test]
fn a_complete_workspace_classifies_every_member() {
    let root = complete_workspace();
    let members = workspace::members(root.path()).expect("members are readable");
    assert_eq!(members.len(), matrix::all().len());
    assert_eq!(
        boundary::coverage_violations(&members),
        Vec::<boundary::Violation>::new()
    );
    assert_eq!(matrix::duplicated_packages(), Vec::<&str>::new());
}

/// A forbidden dependency is a forbidden dependency wherever a manifest names it.
/// Every table and every target scope is checked, because a rule that reads only
/// `[dependencies]` is a rule a contributor routes around. `dev-dependencies` and
/// `build-dependencies` are the two that get forgotten, so they are the two that
/// are pinned.
#[rstest]
#[case::dev_dependencies(
    "[dev-dependencies]\nwindows = \"0.62\"\n",
    vec![Rule::ForbiddenDependency]
)]
#[case::build_dependencies(
    "[build-dependencies]\nwindows = \"0.62\"\n",
    vec![Rule::ForbiddenDependency]
)]
#[case::the_repositorys_own_windows_crate(
    "[dependencies]\nzup-windows = { path = \"../zup-windows\" }\n",
    vec![Rule::ForbiddenDependency]
)]
#[case::a_windows_target_scope(
    "[target.'cfg(windows)'.dependencies]\nwindows-link = \"0.100\"\n",
    vec![Rule::ForbiddenDependency, Rule::PlatformTargetScope]
)]
#[case::a_windows_target_scope_over_a_portable_dependency(
    "[target.'cfg(windows)'.dependencies]\nserde = \"1\"\n",
    vec![Rule::PlatformTargetScope]
)]
#[case::the_repositorys_own_linux_crate(
    "[dependencies]\nzup-linux = { path = \"../zup-linux\" }\n",
    vec![Rule::ForbiddenDependency]
)]
#[case::a_linux_target_scope(
    "[target.'cfg(target_os = \"linux\")'.dependencies]\nserde = \"1\"\n",
    vec![Rule::PlatformTargetScope]
)]
#[case::the_linux_syscall_crate(
    "[dependencies]\nrustix = \"1.1\"\n",
    vec![Rule::ForbiddenDependency]
)]
fn a_manifest_that_reaches_for_a_native_backend_is_reported(
    #[case] table: &str,
    #[case] expected: Vec<Rule>,
) {
    let root = workspace_with(
        "zup-core",
        &format!(
            "[package]\nname = \"zup-core\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\n{table}"
        ),
        &[],
    );
    assert_eq!(rules(root.path()), expected);
}

#[test]
fn a_windows_cfg_branch_in_production_source_is_reported() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[(
            "src/lib.rs",
            "pub fn reparse() -> bool {\n    #[cfg(windows)]\n    {\n        true\n    }\n    \
             #[cfg(not(windows))]\n    {\n        false\n    }\n}\n",
        )],
    );
    assert_eq!(rules(root.path()), vec![Rule::PlatformCfgBranch]);
    let reported = findings(root.path());
    assert_eq!(reported.len(), 2, "{reported:?}");
    assert!(
        reported[0].contains("zup-core/src/lib.rs:2"),
        "{reported:?}"
    );
    assert!(reported[0].contains("cfg(windows)"), "{reported:?}");
    assert!(
        reported[1].contains("zup-core/src/lib.rs:6"),
        "{reported:?}"
    );
    assert!(reported[1].contains("cfg(not(windows))"), "{reported:?}");
}

/// The Linux half of the same rule. A portable crate that branched on the host
/// operating system would be exactly as platform-shaped as one that branched on
/// the host architecture family, and only one of them would be caught.
#[rstest]
#[case::the_operating_system("#[cfg(target_os = \"linux\")]\npub fn f() {}\n")]
#[case::the_target_family("#[cfg(target_family = \"unix\")]\npub fn f() {}\n")]
#[case::the_posix_predicate("#[cfg(unix)]\npub fn f() {}\n")]
#[case::its_negation("#[cfg(not(unix))]\npub fn f() {}\n")]
fn a_linux_cfg_branch_in_production_source_is_reported(#[case] source: &str) {
    let root = workspace_with("zup-core", &manifest("zup-core"), &[("src/lib.rs", source)]);
    assert_eq!(
        rules(root.path()),
        vec![Rule::PlatformCfgBranch],
        "{source}"
    );
}

/// An import nested inside a function is still an import. A scanner that only
/// matched at the top of a file would let every portable crate re-acquire the
/// platform filesystem API behind one level of indentation.
#[rstest]
#[case::windows(
    "pub fn attributes() -> u32 {\n    use std::os::windows::fs::MetadataExt;\n    0\n}\n"
)]
#[case::unix("pub fn attributes() -> u32 {\n    use std::os::unix::fs::MetadataExt;\n    0\n}\n")]
fn a_platform_specific_std_import_is_reported(#[case] source: &str) {
    let root = workspace_with("zup-core", &manifest("zup-core"), &[("src/lib.rs", source)]);
    assert_eq!(rules(root.path()), vec![Rule::OsPlatformImport], "{source}");
}

/// A ban that matched the `windows` crate exactly would be routed around by
/// `windows_bindgen` or `winapi`, so the namespace rule is a prefix rule - and
/// the same has to hold on the other side, or the Linux backend's syscall
/// surface is one alias away from a portable crate.
#[rstest]
#[case::a_crate_that_starts_with_the_windows_prefix("use windows_bindgen::Generator;")]
#[case::the_legacy_crate("use winapi::um::winbase;")]
fn a_windows_api_namespace_is_reported(#[case] token: &str) {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[("src/lib.rs", &format!("{token}\n"))],
    );
    assert_eq!(
        rules(root.path()),
        vec![Rule::NativeApiNamespace],
        "{token}"
    );
}

#[test]
fn every_banned_portable_identifier_is_reported() {
    for banned in boundary::BANNED_IDENTIFIERS {
        let root = workspace_with(
            "zup-core",
            &manifest("zup-core"),
            &[("src/lib.rs", &format!("pub struct {banned} {{}}\n"))],
        );
        assert_eq!(
            rules(root.path()),
            vec![Rule::BannedIdentifier],
            "{banned} was not reported"
        );
    }
}

/// The exemption for a native backend must be an exemption, not a gap. A rule
/// that failed to fire on a backend would also fail to fire on a portable crate
/// that merely claimed to be one, which is why the matrix is what grants the
/// exemption and not the crate's own say-so.
#[rstest]
#[case::windows(
    "zup-windows",
    "[target.'cfg(windows)'.dependencies]\nwindows = \"0.62\"\n",
    "use std::os::windows::fs::MetadataExt;\n\
     pub struct Shortcut;\n\
     pub fn shell() -> windows::Win32::Foundation::HANDLE { todo!() }\n"
)]
#[case::linux(
    "zup-linux",
    "[target.'cfg(target_os = \"linux\")'.dependencies]\nrustix = \"1.1\"\n",
    "use rustix::fs::stat;\n\
     use std::os::unix::fs::MetadataExt;\n\
     pub struct SystemdUnit;\n\
     pub fn unit() -> String { String::from(\"acme.service\") }\n"
)]
fn a_native_backend_package_may_use_everything(
    #[case] package: &str,
    #[case] table: &str,
    #[case] source: &str,
) {
    let root = workspace_with(
        package,
        &format!("{}{table}", manifest(package)),
        &[("src/lib.rs", source)],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

/// The two backends are siblings, and the exemption is what each one holds for
/// itself. A portable crate that reached into either is the failure this whole
/// module exists to prevent, so it is pinned on both sides at once.
#[test]
fn a_portable_crate_may_not_reach_either_backend() {
    for backend in matrix::backends() {
        let root = workspace_with(
            "zup-core",
            &format!(
                "[package]\nname = \"zup-core\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\
                 \n[dependencies]\n{backend} = {{ path = \"../{backend}\" }}\n"
            ),
            &[],
        );
        let reported = findings(root.path());
        assert_eq!(reported.len(), 1, "{backend}: {reported:?}");
        assert!(
            reported[0].contains(&format!("declares {backend}")),
            "{backend}: {reported:?}"
        );
    }
}

/// The declared permission relaxation is only worth having if it is narrow. A
/// package holding it may reach the Unix mode API, and nothing else it did not
/// hold before: not a Windows permission model, and not a native backend.
#[test]
fn a_permission_relaxation_does_not_become_a_platform_exemption() {
    let relaxed = workspace_with(
        "zup-preview",
        &manifest("zup-preview"),
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\npub fn run(path: &std::path::Path) {\n    \
             use std::os::unix::fs::PermissionsExt;\n    \
             let mode = std::fs::metadata(path).unwrap().permissions().mode();\n    \
             let _ = mode | 0o100;\n}\n",
        )],
    );
    assert_eq!(findings(relaxed.path()), Vec::<String>::new());

    let widened = workspace_with(
        "zup-preview",
        &manifest("zup-preview"),
        &[("src/lib.rs", "#[cfg(windows)]\npub fn f() {}\n")],
    );
    assert_eq!(
        rules(widened.path()),
        vec![Rule::PlatformCfgBranch],
        "the relaxation buys a mode bit, not a choice between two backends"
    );

    let reached = workspace_with(
        "zup-preview",
        &format!(
            "{}\n[dependencies]\nrustix = \"1.1\"\n",
            manifest("zup-preview")
        ),
        &[],
    );
    assert_eq!(
        rules(reached.path()),
        vec![Rule::ForbiddenDependency],
        "and it grants no backend dependency either"
    );
}

/// Test code names the vocabulary it is testing, so a `#[cfg(test)]` module and a
/// `tests/` integration file are held to no rules at all. Without this, every
/// crate that asserts "this token is banned" would itself be a finding.
#[test]
fn unit_test_modules_and_test_directories_may_use_windows() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[
            (
                "src/lib.rs",
                "pub fn value() -> u8 { 1 }\n\
                 \n#[cfg(test)]\nmod tests {\n    use super::*;\n    \
                 #[test]\n    fn schema() {\n        for banned in [\"RegistryHive\"] {\n            \
                 assert!(!banned.is_empty());\n        }\n    }\n}\n",
            ),
            (
                "tests/windows.rs",
                "#![cfg(windows)]\nuse std::os::windows::fs::MetadataExt;\n\
                 pub struct UninstallEntry;\n",
            ),
        ],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

/// The scrubber is what makes the rest of the vocabulary rules usable. Without it
/// a crate could document the vocabulary it is forbidden from, and a doc comment
/// naming a Windows API would be indistinguishable from a call to one.
#[test]
fn comments_do_not_trip_a_rule() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[(
            "src/lib.rs",
            "//! This crate never calls Win32:: and never uses std::os::windows::fs.\n\
             /// `#[cfg(windows)]` appears here only as documentation.\n\
             /// The banned vocabulary includes HKEY_LOCAL_MACHINE, ProgId, vcredist.\n\
             /* A block comment may name the escaped \\\\.\\\\pipe\\\\Acme spelling. */\n\
             pub const PORTABLE_ONLY: &str = \"never used\";\n",
        )],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

/// Every token the ban table lists must fire when it appears inside a string
/// literal, in the spelling a real manifest would use - a registry path, a shell
/// verb, a pipe endpoint, a redistributable name. The table is the whole check;
/// this walks it in the shapes that are easiest to miss.
#[test]
fn a_windows_concept_in_a_string_literal_is_reported() {
    for literal in [
        r#"pub const KEY: &str = "HKEY_LOCAL_MACHINE\\Software\\Acme";"#,
        r#"pub const COMMAND: &str = r"shell\open\command";"#,
        r#"pub const TOOL: &str = "sc.exe delete Acme";"#,
        r#"pub const ENDPOINT: &str = "\\\\.\\pipe\\Acme";"#,
        r#"pub const ENDPOINT: &str = r"\\.\pipe\Acme";"#,
        r#"pub const RUNTIME: &str = "WebView2 Evergreen Runtime";"#,
        r#"pub const FOLDER: &str = "C:\\ProgramData\\Acme";"#,
    ] {
        let root = workspace_with(
            "zup-core",
            &manifest("zup-core"),
            &[("src/lib.rs", &format!("{literal}\n"))],
        );
        assert_eq!(
            rules(root.path()),
            vec![Rule::BannedLiteral],
            "{literal} was not reported"
        );
    }
}

/// The ban table is compared case-insensitively, so a contributor routes around it
/// by lower-casing. The report also has to carry a line number, or a workspace
/// with one leak in it is a workspace nobody can fix.
#[test]
fn a_banned_literal_is_reported_whatever_its_case() {
    for literal in [
        r#"pub const HIVE: &str = "hkey_local_machine";"#,
        r#"pub const HIVE: &str = r"hkey_local_machine";"#,
    ] {
        let root = workspace_with(
            "zup-core",
            &manifest("zup-core"),
            &[("src/lib.rs", &format!("{literal}\n"))],
        );
        let reported = findings(root.path());
        assert_eq!(reported.len(), 1, "{literal}: {reported:?}");
        assert!(
            reported[0].contains("zup-core/src/lib.rs:1"),
            "{literal}: {reported:?}"
        );
        assert!(reported[0].contains("hkey_local_machine"), "{reported:?}");
    }
}

#[test]
fn every_banned_portable_literal_is_reported() {
    for banned in boundary::BANNED_LITERALS {
        // A raw literal spells a token verbatim, including the backslashes the
        // table stores in their decoded form.
        let root = workspace_with(
            "zup-core",
            &manifest("zup-core"),
            &[(
                "src/lib.rs",
                &format!("pub const LEAK: &str = r\"prefix {banned} suffix\";\n"),
            )],
        );
        assert_eq!(
            rules(root.path()),
            vec![Rule::BannedLiteral],
            "{banned} was not reported"
        );
    }
}

/// The ban table cannot be a substring ban. A portable crate has to be able to
/// name `windows` as a target, a triple, and a path segment, or the vocabulary
/// this repository uses is itself illegal.
#[test]
fn the_portable_vocabulary_that_shares_a_windows_word_stays_legal() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[(
            "src/lib.rs",
            "use zup_core::{InstallLocation, Privilege, ServiceStart};\n\
             pub const PLATFORM: &str = \"windows\";\n\
             pub const ARTIFACTS: &str = \"artifacts/{version}/windows-{arch}/Acme-Setup.exe\";\n\
             pub const TRIPLE: &str = \"x86_64-pc-windows-msvc\";\n\
             pub fn location() -> InstallLocation { InstallLocation::Desktop }\n\
             pub fn privilege() -> Privilege { Privilege::System }\n\
             pub fn start() -> ServiceStart { ServiceStart::OnInstall }\n\
             pub const CHOICE: &str = \"a | b\";\n\
             pub const HINT: &str = \"elevation is a presentation concern\";\n\
             pub const PLUGIN_ROOT: &str = \"__zup_plugins__\";\n\
             pub const RECEIPT: &str = \"receipt\";\n",
        )],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

/// The enforcer holds the ban table, so it necessarily spells every banned token
/// out. It is exempted from the vocabulary rules, and the exemption has to be
/// narrow: an enforcer that could not name a Windows concept could not check for
/// one.
#[test]
fn the_enforcer_may_name_the_concepts_it_bans() {
    let root = workspace_with(
        boundary::ENFORCER,
        &manifest(boundary::ENFORCER),
        &[(
            "src/lib.rs",
            "pub const TABLE: &[&str] = &[\"registry\", \"ProgId\", \"vcredist\"];\n",
        )],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

#[test]
fn the_enforcer_still_may_not_depend_on_a_windows_crate() {
    let root = workspace_with(
        boundary::ENFORCER,
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\
             \n[dependencies]\nwindows = \"0.62\"\n",
            name = boundary::ENFORCER
        ),
        &[],
    );
    assert_eq!(rules(root.path()), vec![Rule::ForbiddenDependency]);
}

/// An unclassified member is a package no matrix claims, so nothing verifies it
/// and nothing knows which host it builds on. Excluding it is the deliberate way
/// to say "not mine" - but only if the exclusion is actually honoured.
#[rstest]
#[case::a_package_no_matrix_claims(&[], 1)]
#[case::the_same_package_explicitly_excluded(&["crates/zup-scratch"], 0)]
fn an_unclassified_member_is_reported_unless_it_is_excluded(
    #[case] exclude: &[&str],
    #[case] expected_findings: usize,
) {
    let root = complete_workspace();
    add_package(root.path(), "zup-scratch", &manifest("zup-scratch"), &[]);
    write_root(root.path(), &["crates/*"], exclude);
    let reported = findings(root.path());
    assert_eq!(reported.len(), expected_findings, "{reported:?}");
    if expected_findings == 1 {
        assert!(
            reported[0].contains("unclassified workspace member `zup-scratch`"),
            "{reported:?}"
        );
    }
}

#[test]
fn every_member_of_this_workspace_is_classified() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let members = workspace::members(&root).expect("workspace members are readable");
    assert_eq!(
        boundary::coverage_violations(&members),
        Vec::<boundary::Violation>::new(),
        "every member must be in exactly one matrix"
    );
}

#[test]
fn findings_are_ordered_by_matrix_then_package_then_file() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[
            ("src/zzz.rs", "use std::os::windows::fs::MetadataExt;\n"),
            ("src/aaa.rs", "pub struct ProgId;\n"),
        ],
    );
    add_package(
        root.path(),
        "zup-plan",
        &manifest("zup-plan"),
        &[("src/lib.rs", "use windows::Win32::Foundation;\n")],
    );
    let reported = findings(root.path());
    let files: Vec<&str> = reported
        .iter()
        .filter_map(|line| {
            line.split("(crates/")
                .nth(1)?
                .split_once(')')
                .map(|(file, _)| file)
        })
        .collect();
    assert_eq!(
        files,
        vec![
            "zup-core/src/aaa.rs:1",
            "zup-core/src/zzz.rs:1",
            "zup-plan/src/lib.rs:1"
        ],
        "{reported:?}"
    );
}
