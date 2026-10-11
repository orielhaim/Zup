//! `zup preset init` output must resolve and stay on the public API.

use std::path::Path;
use std::process::Command;

use rstest::rstest;

/// A preset outside the workspace reaches the SDK through crates.io.
/// While the SDK is pre-release the tests patch the chain to this checkout.
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

/// The two generators differ only in the crate type they build.
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
fn sdk_chain(role: Generated) -> Vec<(&'static str, &'static str)> {
    let mut chain = vec![("zup-sdk", "zup-sdk")];
    chain.extend(match role {
        Generated::Preset => vec![
            ("zup-sdk-macros", "zup-sdk-macros"),
            ("zup-preset-protocol", "zup-preset-protocol"),
            ("zup-preset-ipc", "zup-preset-ipc"),
        ],
        Generated::Plugin => vec![("zup-plugin-abi", "zup-plugin-abi")],
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

/// A generated project resolves, teaches the public API, and is rustfmt-clean.
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
    // The whole claim: a preset project declares the SDK and nothing else.
    // Not `gpui-kit`, whose version a preset author would have to keep in step
    // with Zup's; not `serde` or `schemars`, whose versions would decide which
    // schema an application's settings are validated against.
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
    for expected in ["zup-sdk", "zup-sdk-macros", "zup-preset-protocol"] {
        assert!(
            resolved.contains(&expected),
            "and Cargo resolved `{expected}` to a real package, from outside this workspace"
        );
    }
}

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
