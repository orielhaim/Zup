//! What a person gets when they run the developer CLI, and what `cargo install`
//! can give them at all.
//!
//! Both are properties of the *binary* and the *package*, not of any function, so
//! they are checked against the built executable and the manifests themselves. A
//! test that called the parser directly would keep passing after a `[[bin]]` was
//! added, or after a second package became installable — and either of those is
//! exactly the regression this pair exists to catch.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The repository root, from this package's manifest.
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/zup is two levels below the root")
        .to_path_buf()
}

/// The text of a manifest, which is TOML and is read as text on purpose.
///
/// A test that parsed it would need a TOML parser in the package it is checking,
/// and a boundary test should not add a dependency to the thing it is a boundary
/// for.
fn manifest(package: &str) -> String {
    let path = repository().join("crates").join(package).join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The name of every `[[bin]]` a manifest declares, in file order.
///
/// Scoped to the `[[bin]]` sections, because `name =` also appears in `[package]`
/// and in `default-run`, and a parser that matched those would report a package
/// name as a binary.
fn binaries(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in source.lines() {
        let line = line.trim();
        if line == "[[bin]]" {
            inside = true;
            continue;
        }
        if line.starts_with('[') {
            inside = false;
            continue;
        }
        if inside
            && let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim().strip_prefix('=')
        {
            names.push(value.trim().trim_matches('"').to_owned());
        }
    }
    names
}

/// Whether `help` offers `verb` as a command, rather than mentioning it in prose.
///
/// A substring test would flag an about line that says "installers", so the line
/// has to look like a command line: an indented name with nothing before it.
fn offers(help: &str, verb: &str) -> bool {
    help.lines().any(|line| {
        let line = line.trim();
        line == verb || line.starts_with(&format!("{verb} "))
    })
}

#[test]
fn the_developer_binary_prints_help_with_no_features() {
    // `zup` declares no features, so this executable is byte-for-byte what
    // `cargo run -- --help` produces. Before the package split, that command
    // needed `--features build` to work at all.
    let output = Command::new(env!("CARGO_BIN_EXE_zup"))
        .arg("--help")
        .output()
        .expect("the developer CLI runs");
    assert!(output.status.success(), "`zup --help` failed");
    let help = String::from_utf8_lossy(&output.stdout);
    for verb in [
        "init",
        "check",
        "doctor",
        "plan",
        "build",
        "artifact",
        "sign",
        "publish",
        "ci",
        "toolchain",
        "schema",
        "fmt",
        "completions",
    ] {
        assert!(offers(&help, verb), "`{verb}` is missing:\n{help}");
    }
    for runtime in [
        "install",
        "upgrade",
        "modify",
        "repair",
        "uninstall",
        "recover",
        "__worker",
    ] {
        assert!(
            !offers(&help, runtime),
            "`{runtime}` is an application runtime verb:\n{help}"
        );
    }
    assert!(
        !help.contains("__worker") && !help.contains("WorkerHelp"),
        "the runtime's process boundaries are not this tool's:\n{help}"
    );
}

#[test]
fn the_developer_package_builds_exactly_one_binary() {
    assert_eq!(
        binaries(&manifest("zup")),
        vec!["zup".to_owned()],
        "`zup` is one executable with one shape; a second `[[bin]]` is a second product"
    );
}

/// `cargo install` builds whatever a package's `[package]` section says it can.
///
/// The installer runtime is a build artifact, not a tool: it is embedded in a
/// generated file and reaches a user's machine that way. Anything that lets a
/// person install one of those binaries onto their `PATH` has turned an internal
/// component into a product with no review in between.
#[test]
fn the_installer_runtime_is_not_installable() {
    let source = manifest("zup-installer");
    assert!(
        source.contains("publish = false"),
        "zup-installer must not be installable:\n{source}"
    );
    let names = binaries(&source);
    assert_eq!(
        names.len(),
        3,
        "three presentations, and no fourth: {names:?}"
    );
    for name in &names {
        assert!(
            name.starts_with("zup-setup-"),
            "{name} is an embedded component, not a tool anybody runs"
        );
    }
}

/// Every workspace member is either the developer tool or not installable.
///
/// Read across the whole workspace rather than from a list, so a package added
/// tomorrow is covered by this test without anyone remembering to extend it.
#[test]
fn only_the_developer_tool_is_installable() {
    let root = manifest_root();
    let mut installable = Vec::new();
    for entry in std::fs::read_dir(&root).expect("the crates directory") {
        let directory = entry.expect("a crates directory entry").path();
        let source = std::fs::read_to_string(directory.join("Cargo.toml"))
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()));
        let name = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .expect("a package directory");
        if name == "zup" {
            continue;
        }
        if source.contains("publish = false") {
            continue;
        }
        if !binaries(&source).is_empty() {
            installable.push(name);
        }
    }
    assert!(
        installable.is_empty(),
        "these packages build a binary and are not marked `publish = false`, so \
         `cargo install` can put them on a user's PATH: {installable:?}"
    );
}

/// The `crates` directory every workspace member lives in.
fn manifest_root() -> PathBuf {
    repository().join("crates")
}
