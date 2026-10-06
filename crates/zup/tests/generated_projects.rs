//! `zup preset init` produces a project a third party can build.
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

use rstest::rstest;

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

/// Every crate the published preset SDK resolves through, in publish order.
fn cargo() -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(env!("CARGO_MANIFEST_DIR"));
    command
}

fn generated(directory: &Path) -> std::path::PathBuf {
    zup::preset::init::init("aurora", directory).expect("the project is generated");
    directory.join("aurora")
}

/// Which generator produced a project, where each one writes its Rust.
///
/// The two differ in the crate type they build and nothing else, and the checks
/// that have to hold for both are the ones worth writing once.
#[derive(Debug, Clone, Copy)]
enum Generated {
    Preset,
    Plugin,
}

impl Generated {
    fn name(self) -> &'static str {
        match self {
            Self::Preset => "preset",
            Self::Plugin => "plugin",
        }
    }

    /// The Rust file a generated project is written as.
    fn source(self) -> &'static str {
        match self {
            Self::Preset => "src/main.rs",
            Self::Plugin => "src/lib.rs",
        }
    }
}

fn plugin(directory: &Path) -> std::path::PathBuf {
    zup::plugin::init(&zup::plugin::PluginInitCommand {
        name: "configure".to_owned(),
        directory: Some(directory.to_path_buf()),
    })
    .expect("the project is generated");
    directory.join("configure")
}

/// Generate a project with the whole SDK chain patched to this checkout.
fn generated_with_local_sdk(directory: &Path) -> std::path::PathBuf {
    let root = generated(directory);
    append_patch(&root, &sdk_chain(Generated::Preset));
    root
}

/// Generate a plugin with the whole SDK chain patched to this checkout.
fn generated_plugin_with_local_sdk(directory: &Path) -> std::path::PathBuf {
    let root = plugin(directory);
    append_patch(&root, &sdk_chain(Generated::Plugin));
    root
}

/// Every crate the published SDK resolves through for one role, in publish order.
///
/// A generated project names only `zup-sdk`, and Cargo still resolves the crates
/// beneath it from crates.io. Listing them is what makes the tests prove the
/// whole chain resolves rather than only the top, and the two roles resolve
/// different chains because they depend on different crates.
fn sdk_chain(role: Generated) -> Vec<(&'static str, &'static str)> {
    let mut chain = vec![("zup-sdk", "zup-sdk")];
    chain.extend(match role {
        Generated::Preset => vec![
            ("zup-preset-sdk", "zup-preset-sdk"),
            ("zup-preset-sdk-macros", "zup-preset-sdk-macros"),
            ("zup-preset-protocol", "zup-preset-protocol"),
            ("zup-preset-ipc", "zup-preset-ipc"),
        ],
        Generated::Plugin => vec![
            ("zup-plugin-sdk", "zup-plugin-sdk"),
            ("zup-plugin-abi", "zup-plugin-abi"),
        ],
    });
    chain
}

/// Point every crate in `chain` at this checkout, in the generated manifest.
fn append_patch(root: &Path, chain: &[(&str, &str)]) {
    let manifest = root.join("Cargo.toml");
    std::fs::write(
        &manifest,
        format!(
            "{}\n{}\n",
            std::fs::read_to_string(&manifest).expect("the manifest"),
            patch(chain)
        ),
    )
    .expect("the patch is appended");
}

/// A generated project is a Cargo project, and Cargo can resolve it.
///
/// Outside this workspace, with nothing but crates.io and the patches. A
/// generated manifest that names a version nothing provides fails here rather
/// than on a new author's machine.
#[test]
fn a_generated_project_resolves_against_the_published_sdk() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated_with_local_sdk(directory.path());

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
    // The whole claim: a preset project declares the SDK and nothing else. Not
    // `gpui-kit`, whose version a preset author would have to keep in step with
    // Zup's; not `serde` or `schemars`, whose versions would decide which schema
    // an application's settings are validated against.
    assert_eq!(declared, ["zup-sdk"]);

    let features = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "zup-sdk")
        .expect("the SDK is in the graph")["features"]
        .as_object()
        .expect("the SDK declares features");
    assert!(
        features["preset"].is_array(),
        "`preset` is one of the SDK's authoring features: {features:?}"
    );
    assert!(
        features["plugin"].is_array(),
        "and so is `plugin`, because a preset is not the only thing one can author"
    );
    assert!(
        features.get("default").is_none(),
        "with no default feature, so `zup-sdk` alone is a mistake rather than a \
         silently empty dependency: {features:?}"
    );

    let resolved: Vec<&str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|package| package["name"].as_str().expect("a name"))
        .collect();
    for expected in ["zup-sdk", "zup-preset-sdk", "zup-preset-protocol"] {
        assert!(
            resolved.contains(&expected),
            "and Cargo resolved `{expected}` to a real package, from outside this workspace"
        );
    }
}

/// A preset author depends on the SDK for the GPUI stack too.
///
/// This is what makes `zup-sdk` alone a sufficient dependency rather than a
/// starting point: the crates a preset needs to write settings and draw a window
/// are all reachable through it.
#[test]
fn the_generated_dependency_reaches_gpu_i_without_naming_it() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated_with_local_sdk(directory.path());

    let output = cargo()
        .arg("tree")
        .arg("--edges=normal")
        .arg("--prefix=none")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "the generated project has a graph: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph = String::from_utf8_lossy(&output.stdout);
    assert!(
        graph.contains("gpui-kit"),
        "a preset draws with GPUI, and it gets it through the SDK rather than by \
         declaring a version of its own"
    );
    assert!(
        !graph.contains("wasmtime"),
        "and the plugin runtime is nowhere in a preset's graph: it is a different \
         authoring role entirely"
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
        source.contains("zup_sdk::preset::run::<Aurora>()"),
        "one call starts it, through the crate a preset author depends on"
    );
    assert!(
        !source.contains("impl Preset for Preset {"),
        "and the struct is not called `Preset`, which would shadow the trait in \
         the position that names it"
    );
    assert!(
        !source.contains("zup_preset_sdk::"),
        "and it never names the crate the SDK is built from: the implementation \
         behind the facade is not something a preset has to know"
    );
    assert!(
        source.contains("#[zup_sdk::preset::settings]"),
        "with a settings type the SDK has already given the derives it needs"
    );
    assert!(
        !source.contains("serde::Deserialize") && !source.contains("schemars::JsonSchema"),
        "so the project depends on neither crate directly"
    );
    assert!(
        source.contains("AssetRef"),
        "and the type that marks a setting as a file"
    );
    assert!(
        source.contains("session.send(Action::Install)"),
        "and asks the host for things"
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

/// Generated Rust is formatted Rust.
///
/// A template is a string, so nothing in the build complains when two of its
/// lines are joined: the first thing an author sees is a window full of
/// indentation that is wrong in a way that looks like their mistake. This is
/// `rustfmt --check` run against what the generator actually produced, so a
/// template that stops being canonical source is a failure here rather than in
/// the first project someone generates.
#[rstest]
#[case::preset(Generated::Preset)]
#[case::plugin(Generated::Plugin)]
fn generated_rust_is_formatted(#[case] kind: Generated) {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = match kind {
        Generated::Preset => generated(directory.path()),
        Generated::Plugin => plugin(directory.path()),
    };

    let output = rustfmt()
        .arg("--edition")
        .arg("2024")
        .arg("--check")
        .arg(root.join(kind.source()))
        .output()
        .expect("rustfmt runs");
    assert!(
        output.status.success(),
        "a generated {} is rustfmt-clean:\n{}",
        kind.name(),
        String::from_utf8_lossy(&output.stdout)
    );
}

fn rustfmt() -> Command {
    let mut command = Command::new(std::env::var_os("RUSTFMT").unwrap_or_else(|| "rustfmt".into()));
    command.current_dir(env!("CARGO_MANIFEST_DIR"));
    command
}

/// The development document is a source file, not development state.
///
/// It is the thing a preset author edits, so it belongs in the same commit as the
/// code it exercises, and `.zup` is the thing the process writes and throws away.
#[test]
fn a_generated_project_says_which_of_its_files_are_its_own() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated(directory.path());

    let development =
        std::fs::read_to_string(root.join("zup.preset.dev.toml")).expect("the document");
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
        !ignored.contains("zup.preset.dev.toml"),
        "while the development document is: it is the thing being edited"
    );
}

/// A generated preset compiles.
///
/// The generated source is the first thing a preset author ever compiles, and a
/// template that does not build is the most expensive failure this crate can
/// have: it is found by the first person to use it, on their machine, with
/// nothing to point at. Checking that it compiles is therefore not optional, and
/// not something a manifest resolution can stand in for.
///
/// Ignored because it compiles the GPUI stack, which is minutes rather than
/// seconds. Run it with `cargo test -p zup --test generated_projects -- --ignored`
/// before changing the template.
#[test]
#[ignore = "compiles the GPUI stack, which takes longer than the rest of this suite"]
fn a_generated_project_compiles() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated_with_local_sdk(directory.path());
    let output = cargo()
        .current_dir(&root)
        .arg("check")
        .arg("--all-targets")
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "a generated preset builds:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("warning:"),
        "and builds without warnings, because the first warning an author sees is \
         one they will think they caused:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A generated plugin compiles to a component.
///
/// The other half of the claim, and the one a plugin author cannot check without
/// learning the Component Model first: the crate builds for the guest target and
/// componentises.
#[test]
#[ignore = "compiles the bindings generator for Wasm, which takes a while"]
fn a_generated_plugin_compiles_and_componentises() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated_plugin_with_local_sdk(directory.path());

    let output = cargo()
        .current_dir(&root)
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "a generated plugin builds for Wasm:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let component = root.join("target").join("wasm32-unknown-unknown");
    let component = component.join("release").join("configure.wasm");
    let bytes = std::fs::read(&component).expect("cargo produced a module");
    let componentised = zup_plugin_build::componentize(&bytes)
        .expect("the module componentises against the contract");
    assert_eq!(
        &componentised[..4],
        &[0x00, 0x61, 0x73, 0x6d],
        "and what comes out is a component rather than a core module"
    );
}

/// `zup plugin init` produces a project a third party can build too.
///
/// The two generators differ in what they write and nothing else: both produce a
/// project whose only dependency is `zup-sdk`, and both are checked here so the
/// plugin one cannot drift into needing a toolchain of its own.
#[test]
fn a_generated_plugin_resolves_against_the_published_sdk() {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = generated_plugin_with_local_sdk(directory.path());

    let output = cargo()
        .arg("metadata")
        .arg("--format-version=1")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "the generated plugin resolves: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata is json");
    let package = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "configure")
        .expect("the generated plugin is in its own graph");
    let declared: Vec<&str> = package["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .map(|dependency| dependency["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        declared,
        ["zup-sdk"],
        "a plugin author declares the SDK and nothing else - not wit-bindgen, and \
         not a vendored copy of the contract"
    );

    // A plugin is a `cdylib` rather than a binary: it is compiled to Wasm and
    // becomes a component, not something a person runs.
    let targets = package["targets"].as_array().expect("targets");
    let library = targets
        .iter()
        .find(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
        })
        .expect("the generated plugin builds a cdylib");
    assert_eq!(library["name"], "configure");

    let source = std::fs::read_to_string(root.join("src/lib.rs")).expect("the source");
    assert!(
        source.contains("zup_sdk::plugin::export!"),
        "and one call turns it into a component"
    );
    assert!(
        !source.contains("wit_bindgen") && !source.contains("wit/"),
        "with no bindings generator and no copy of the contract in the project"
    );
}
