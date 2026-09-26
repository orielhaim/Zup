//! Boundary fixtures.
//!
//! Every fixture is a complete workspace on disk: one member per package the
//! matrices name, plus the `crates/*` member glob the repository uses. A
//! fixture then dirties exactly one package, and the matrix-membership rule
//! stays satisfied.

use std::fs;
use std::path::Path;

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

#[test]
fn a_windows_dependency_in_any_manifest_table_is_reported() {
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let root = workspace_with(
            "zup-core",
            &format!(
                "[package]\nname = \"zup-core\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\
                 \n[{table}]\nwindows = \"0.62\"\n"
            ),
            &[],
        );
        let reported = rules(root.path());
        assert!(
            reported.contains(&Rule::ForbiddenDependency),
            "{table} was not reported: {reported:?}"
        );
    }
}

#[test]
fn a_windows_target_scope_reports_both_the_scope_and_the_dependency() {
    let root = workspace_with(
        "zup-core",
        "[package]\nname = \"zup-core\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\
         \n[target.'cfg(windows)'.dependencies]\nwindows-link = \"0.100\"\n",
        &[],
    );
    assert_eq!(
        rules(root.path()),
        vec![Rule::ForbiddenDependency, Rule::WindowsTargetScope]
    );
}

#[test]
fn a_windows_target_scope_without_windows_dependencies_is_reported() {
    let root = workspace_with(
        "zup-core",
        "[package]\nname = \"zup-core\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\
         \n[target.'cfg(windows)'.dependencies]\nserde = \"1\"\n",
        &[],
    );
    assert_eq!(rules(root.path()), vec![Rule::WindowsTargetScope]);
}

#[test]
fn the_windows_backend_crate_itself_is_reported_as_a_dependency() {
    let root = workspace_with(
        "zup-core",
        "[package]\nname = \"zup-core\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\
         \n[dependencies]\nzup-windows = { path = \"../zup-windows\" }\n",
        &[],
    );
    assert_eq!(rules(root.path()), vec![Rule::ForbiddenDependency]);
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
    assert_eq!(rules(root.path()), vec![Rule::WindowsCfgBranch]);
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

#[test]
fn a_platform_specific_std_import_is_reported() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[(
            "src/lib.rs",
            "pub fn attributes() -> u32 {\n    use std::os::windows::fs::MetadataExt;\n    0\n}\n",
        )],
    );
    assert_eq!(rules(root.path()), vec![Rule::OsWindowsImport]);
}

#[test]
fn a_windows_api_namespace_is_reported() {
    for token in [
        "use windows::Win32::System::SystemInformation;",
        "use windows_bindgen::Generator;",
        "use winapi::um::winbase;",
    ] {
        let root = workspace_with(
            "zup-core",
            &manifest("zup-core"),
            &[("src/lib.rs", &format!("{token}\n"))],
        );
        assert_eq!(
            rules(root.path()),
            vec![Rule::WindowsApiNamespace],
            "{token} was not reported"
        );
    }
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

#[test]
fn a_windows_only_package_may_use_everything() {
    let root = workspace_with(
        "zup-windows",
        &format!(
            "{}[target.'cfg(windows)'.dependencies]\nwindows = \"0.62\"\n",
            manifest("zup-windows")
        ),
        &[(
            "src/lib.rs",
            "use std::os::windows::fs::MetadataExt;\n\
             pub struct Shortcut;\n\
             pub fn shell() -> windows::Win32::Foundation::HANDLE { todo!() }\n",
        )],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

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

#[test]
fn a_windows_concept_in_a_string_literal_is_reported() {
    for literal in [
        r#"pub const KEY: &str = "HKEY_LOCAL_MACHINE\\Software\\Acme";"#,
        r#"pub const HIVE: &str = "HKEY_CURRENT_USER";"#,
        r#"pub const COMMAND: &str = r"shell\open\command";"#,
        r#"pub const CLASS: &str = "Acme.Shell.1"; pub const KEY: &str = "ProgId";"#,
        r#"pub const TOOL: &str = "sc.exe delete Acme";"#,
        r#"pub const VERB: &str = "runas";"#,
        r#"pub const ENDPOINT: &str = "\\\\.\\pipe\\Acme";"#,
        r#"pub const ENDPOINT: &str = r"\\.\pipe\Acme";"#,
        r#"pub const PACKAGE: &str = "Acme.msi";"#,
        r#"pub const RUNTIME: &str = "WebView2 Evergreen Runtime";"#,
        r#"pub const REDIST: &str = "vcredist.x64.exe";"#,
        r#"pub const RESOURCE: &str = "RCDATA";"#,
        r#"pub const FOLDER: &str = "C:\\ProgramData\\Acme";"#,
        r#"pub const REGISTRATION: &str = "UninstallString";"#,
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

#[test]
fn a_banned_literal_is_reported_whatever_its_case() {
    for literal in [
        r#"pub const HIVE: &str = "hkey_local_machine";"#,
        r#"pub const HIVE: &str = "HKey_Local_Machine";"#,
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
fn a_banned_literal_outside_a_string_is_not_a_banned_literal() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[(
            "src/lib.rs",
            "// hkey_local_machine is spelled out in this comment.\n\
             pub fn hive() -> &'static str {\n    let key = \"acme\";\n    key\n}\n",
        )],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
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

#[test]
fn target_lexicon_windows_identifiers_are_allowed() {
    let root = workspace_with(
        "zup-core",
        &manifest("zup-core"),
        &[(
            "src/lib.rs",
            "use zup_core::{TargetOperatingSystem, TargetTriple};\n\
             pub fn is_windows(target: &TargetTriple) -> bool {\n    \
             target.operating_system() == TargetOperatingSystem::Windows\n}\n",
        )],
    );
    assert_eq!(findings(root.path()), Vec::<String>::new());
}

#[test]
fn an_unclassified_member_is_reported() {
    let root = complete_workspace();
    add_package(root.path(), "zup-scratch", &manifest("zup-scratch"), &[]);
    let reported = findings(root.path());
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(
        reported[0].contains("unclassified workspace member `zup-scratch`"),
        "{reported:?}"
    );
}

#[test]
fn an_excluded_member_is_not_a_finding() {
    let root = complete_workspace();
    add_package(root.path(), "zup-scratch", &manifest("zup-scratch"), &[]);
    write_root(root.path(), &["crates/*"], &["crates/zup-scratch"]);
    assert_eq!(findings(root.path()), Vec::<String>::new());
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
