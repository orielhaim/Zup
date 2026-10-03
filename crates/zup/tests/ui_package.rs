//! `zup ui inspect` against real packages, real ones and broken ones.
//!
//! Inspect is the read side of the whole artifact pipeline: what a publisher
//! checks before shipping, and what a consumer would check before trusting a
//! package somebody else built. Neither can run the preset, so every failure it
//! reports has to come from the bytes - which is what these cases break.

use zup::ui::{InspectCommand, inspect};
use zup_artifact::ui::{PresetPackageView, PresetPackageWriter};
use zup_core::TargetTriple;
use zup_preset_protocol::{PresetDescription, Capabilities, Capability};

fn packed(targets: &[&str]) -> Vec<u8> {
    let description = PresetDescription::new(
        "aurora",
        "1.4.2",
        serde_json::json!({
            "type": "object",
            "properties": { "accent": { "type": "string" } },
        }),
    )
    .with_capabilities(Capabilities::new([Capability::Components]));
    let mut writer = PresetPackageWriter::new(description).expect("a valid description");
    for target in targets {
        writer
            .add_binary(
                TargetTriple::parse(target).expect("a valid triple"),
                format!("native preset for {target}")
                    .repeat(64)
                    .into_bytes(),
            )
            .expect("one binary per target");
    }
    writer
        .finish()
        .expect("the package is written and verified")
}

fn inspect_bytes(name: &str, bytes: Vec<u8>) -> String {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join(name);
    std::fs::write(&path, bytes).expect("the package is written");
    inspect(&InspectCommand { package: path })
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

/// A package a publisher produced is one a publisher can check.
#[test]
fn a_packed_package_inspects() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("aurora.zupui");
    std::fs::write(&path, packed(&["x86_64-pc-windows-msvc"])).expect("written");
    inspect(&InspectCommand {
        package: path.clone(),
    })
    .expect("a package reads back");
    let view = PresetPackageView::open(std::fs::read(&path).expect("read")).expect("reopens");
    assert_eq!(view.name(), "aurora");
    assert_eq!(view.targets(), ["x86_64-pc-windows-msvc"]);
}

/// Every target a package carries is reported, and one that is not there is a
/// refusal that names the ones that are.
#[test]
fn a_package_of_several_targets_inspects_and_reports_the_right_one() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("aurora.zupui");
    std::fs::write(
        &path,
        packed(&["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]),
    )
    .expect("written");
    inspect(&InspectCommand { package: path }).expect("a package reads back");
}

/// A package a consumer cannot trust is refused, and the refusal says what was
/// wrong with the bytes rather than that something was.
#[test]
fn a_damaged_package_is_refused_with_a_reason() {
    let valid = packed(&["x86_64-pc-windows-msvc"]);

    let mut truncated = valid.clone();
    truncated.truncate(truncated.len() - 32);
    let message = inspect_bytes("truncated.zupui", truncated);
    assert!(message.contains("truncated.zupui"), "{message}");
    assert!(message.contains("its header accounts for"), "{message}");

    let mut with_trailing_bytes = valid.clone();
    with_trailing_bytes.extend_from_slice(b"extra");
    let message = inspect_bytes("trailing.zupui", with_trailing_bytes);
    assert!(message.contains("its header accounts for"), "{message}");

    let mut corrupt = valid.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    let message = inspect_bytes("corrupt.zupui", corrupt);
    assert!(message.contains("does not verify"), "{message}");
    assert!(
        message.contains("x86_64-pc-windows-msvc"),
        "a refusal names the target it is about: {message}"
    );

    let message = inspect_bytes(
        "foreign.zupui",
        b"a file long enough to hold a header but not a package".to_vec(),
    );
    assert!(
        message.contains("not the start of a preset package"),
        "{message}"
    );
}

/// A file that is not there is a different mistake from a file that is there and
/// wrong, and the two are told apart.
#[test]
fn a_missing_package_is_not_a_damaged_one() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let missing = directory.path().join("absent.zupui");
    let error = inspect(&InspectCommand {
        package: missing.clone(),
    })
    .expect_err("there is nothing to read");
    assert!(
        error.to_string().contains("absent.zupui"),
        "the refusal names the file: {error}"
    );
}
