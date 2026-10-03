//! `zup ui init` produces a project a third party can build.
//!
//! The SDK is a crate other people depend on, and the project that teaches them
//! to use it is the first thing most of them will compile. If its manifest names
//! a version that does not exist, or a dependency the SDK does not re-export,
//! every new preset author finds out at the same moment and for the same reason.
//!
//! So the generated project is resolved here, by Cargo, from a directory outside
//! this workspace. A generated `main.rs` is compiled by hand rather than here,
//! because the GPUI stack behind it takes longer to check than this suite is
//! worth; what this file proves is the part that can silently be wrong - the
//! manifest, the dependency versions, and the generated sources.

use std::path::Path;
use std::process::Command;

/// The SDK's own crates, resolved from this checkout.
///
/// A preset outside the workspace reaches the SDK through crates.io, and that is
/// the spelling the generated project carries. While the SDK is pre-release
/// there is nothing on the registry for it to resolve to, so the test does what
/// a third-party author does in that situation: Cargo's own patch mechanism,
/// pointed at the local checkout. Nothing about the generated project changes -
/// it names published versions, and this is where those versions come from for
/// the duration of a test.
fn patch(crates: &[(&str, &str)]) -> String {
    let entries: Vec<String> = crates
        .iter()
        .map(|(name, directory)| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("the workspace crates")
                .join(directory);
            format!(
                "{name} = {{ path = \"{}\" }}",
                path.to_string_lossy().replace('\\', "/")
            )
        })
        .collect();
    format!("[patch.crates-io]\n{}\n", entries.join("\n"))
}

fn cargo() -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(env!("CARGO_MANIFEST_DIR"));
    command
}

fn generated(directory: &Path) -> std::path::PathBuf {
    zup::ui::init::init("aurora", directory).expect("the project is generated");
    directory.join("aurora")
}

/// A generated project is a Cargo project, and Cargo can resolve it.
///
/// Outside this workspace, with nothing but crates.io and the two patches. A
/// generated manifest that names a version nothing provides fails here rather
/// than on a new author's machine.
#[test]
fn a_generated_project_resolves_against_the_published_sdk() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated(directory.path());

    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "{}\n{}\n",
            std::fs::read_to_string(root.join("Cargo.toml")).expect("the manifest"),
            patch(&[
                ("zup-preset-sdk", "zup-preset-sdk"),
                ("zup-preset-protocol", "zup-preset-protocol"),
            ])
        ),
    )
    .expect("the patch is appended");

    let output = cargo()
        .arg("metadata")
        .arg("--format-version=1")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "the generated project resolves: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata is json");
    let package = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "aurora")
        .expect("the generated project is in its own graph");
    assert_eq!(package["version"], "0.1.0");
    let declared: Vec<&str> = package["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .map(|dependency| dependency["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(declared, ["gpui-kit", "schemars", "serde", "zup-preset-sdk"]);
    let resolved: Vec<&str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|package| package["name"].as_str().expect("a name"))
        .collect();
    assert!(
        resolved.contains(&"zup-preset-sdk"),
        "and Cargo resolved the SDK to a real package, from outside this workspace"
    );
}

/// A generated project teaches the public API and nothing else.
///
/// A preset is a normal Rust program that happens to talk to an installer, so the
/// generated source is measured: it uses the SDK's own entry point, it declares
/// an ordinary settings type, and it contains none of the vocabulary a framework
/// around GPUI would have introduced.
#[test]
fn a_generated_project_teaches_the_public_api_and_nothing_else() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated(directory.path());
    let source = std::fs::read_to_string(root.join("src/main.rs")).expect("the source");

    assert!(
        source.contains("zup_preset_sdk::run::<Preset>()"),
        "one call starts it"
    );
    assert!(
        source.contains("type Settings = Settings"),
        "with its own settings type"
    );
    assert!(
        source.contains("AssetRef"),
        "and the type that marks a setting as a file the application provides"
    );
    // A component is the installer's own word for something a person chooses, so
    // the generated window uses it. What it must not contain is a *framework*
    // around GPUI: no wrapper type, no layout file, no screen abstraction the
    // author has to learn before drawing anything.
    for invented in [
        "ZupComponent",
        "PresetLayout",
        "LayoutFile",
        "struct Screen",
        "trait Screen",
        "impl Screen",
        "preset.toml",
    ] {
        assert!(
            !source.contains(invented),
            "a preset is normal Rust, so the generated source names no `{invented}`"
        );
    }
    let lines = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert!(
        lines < 140,
        "and it is small enough to read in one sitting: {lines} non-blank lines"
    );
}

/// The development document is a source file, not development state.
///
/// It is the thing a preset author edits, so it belongs in the same commit as the
/// code it exercises, and `.zup` is the thing the process writes and throws away.
#[test]
fn a_generated_project_says_which_of_its_files_are_its_own() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated(directory.path());

    let development = std::fs::read_to_string(root.join("zup.ui.dev.toml")).expect("the document");
    assert!(
        development.contains("[settings]"),
        "with settings to try the preset against"
    );
    assert!(
        development.contains("[assets]"),
        "and the files an application provides"
    );

    let ignored = std::fs::read_to_string(root.join(".gitignore")).expect("the ignore file");
    assert!(ignored.contains("/target"), "build products are not source");
    assert!(
        ignored.contains("/.zup"),
        "and neither is a development session's own state"
    );
    assert!(
        !ignored.contains("zup.ui.dev.toml"),
        "while the development document is: it is the thing being edited"
    );
}
