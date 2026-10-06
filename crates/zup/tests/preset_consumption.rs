//! Consuming a `.zupui`: what a build proves about it, and what it refuses.
//!
//! The format itself is proved by `zup-artifact`; this is the consumer half. A
//! build takes a package somebody else published, and the question is what it
//! will accept on that application's behalf: a package it cannot read, a target
//! it does not carry, a protocol it cannot speak, a capability this application
//! cannot provide, settings the preset's own schema rejects, and assets that are
//! not files inside the project.
//!
//! Nothing here executes a preset, and nothing here runs a compiler. That is the
//! property being tested: every one of these decisions is reached from the
//! package's bytes and the project's own files.

use sha2::Digest;

use zup_artifact::preset::{PresetPackageView, PresetPackageWriter};
use zup_core::{
    Component, ComponentId, Frontend, Install, InstallDirectory, InstallScope, Installer,
    NonEmptyString, TargetTriple, Template,
};
use zup_platform::PortableSourceFilePolicy;
use zup_preset_compose::{PresetProblem, asset_settings, select, validate_settings};
use zup_preset_protocol::{Capabilities, Capability, PresetDescription};

const HOST: &str = zup_plugin_contract::HOST_TARGET;

fn target(name: &str) -> TargetTriple {
    TargetTriple::parse(name).expect("a valid target triple")
}

/// The settings schema a real preset generates, including one setting that names
/// a file the application provides.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Settings",
        "type": "object",
        "properties": {
            "hero": { "type": ["string", "null"] },
            "accent": { "type": ["string", "null"] },
            "logo": {
                "anyOf": [
                    { "$ref": "#/$defs/AssetRef" },
                    { "type": "null" }
                ]
            }
        },
        "required": ["accent"],
        "$defs": {
            "AssetRef": {
                "type": "string",
                "title": "Asset",
                "x-zup-asset": true
            }
        }
    })
}

fn description() -> PresetDescription {
    PresetDescription::new("aurora", "1.4.2", schema())
        .with_capabilities(Capabilities::new([Capability::Components]))
}

/// A package carrying one binary per named target.
fn package(targets: &[&str]) -> Vec<u8> {
    let mut writer = PresetPackageWriter::new(description()).expect("a valid description");
    for name in targets {
        writer
            .add_binary(
                target(name),
                format!("native preset for {name}").repeat(32).into_bytes(),
            )
            .expect("one binary per target");
    }
    writer.finish().expect("a verified package")
}

/// An application that provides components and nothing else.
fn installer() -> Installer {
    Installer {
        preset: None,
        app: zup_core::App {
            id: zup_core::AppId::new("com.example.consumer").expect("id"),
            name: NonEmptyString::new("Consumer").expect("name"),
            version: semver::Version::parse("1.0.0").expect("version"),
            publisher: None,
            main: None,
            description: None,
        },
        target: target(HOST),
        frontend: Frontend::Gui,
        updates: None,
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse("${location.programs}/Consumer").expect("a template")),
                machine: None,
            },
            allow_directory_override: true,
        },
        prerequisites: Vec::new(),
        components: vec![Component {
            id: ComponentId::new("core").expect("id"),
            name: NonEmptyString::new("Core").expect("name"),
            description: None,
            required: true,
            default: true,
            requires: Vec::new(),
            group: None,
        }],
        component_groups: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    }
}

/// Write a package into a directory and return where it is.
fn write_package(directory: &std::path::Path, targets: &[&str]) -> std::path::PathBuf {
    let path = directory.join("aurora.zupui");
    std::fs::write(&path, package(targets)).expect("the package is written");
    path
}

fn settings(value: serde_json::Value) -> serde_json::Value {
    value
}

/// A package a build can consume is selected, verified, and its settings accepted.
#[test]
fn a_user_package_is_selected_and_its_settings_accepted() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    let installer = installer();

    let selected = select(
        &path,
        &installer,
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect("this application can present this package");

    assert_eq!(selected.name, "aurora");
    assert_eq!(selected.version.to_string(), "1.4.2");
    assert_eq!(
        selected.protocol,
        zup_preset_protocol::PRESET_PROTOCOL_VERSION
    );
    assert_eq!(
        selected.executable,
        format!("native preset for {HOST}").repeat(32).into_bytes(),
        "the bytes composed are the bytes the package carried for this target"
    );
    assert_eq!(
        selected.asset_settings,
        ["logo"],
        "the package's own schema says which settings are files"
    );
}

/// A package that is not a package is refused before anything is read out of it,
/// and so is one that is a package whose bytes do not verify.
#[test]
fn a_damaged_package_is_refused_before_anything_is_read_from_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let installer = installer();
    let good = package(&[HOST]);

    let foreign = directory.path().join("foreign.zupui");
    std::fs::write(&foreign, b"a long enough file that is not a preset at all").expect("written");
    let error = select(
        &foreign,
        &installer,
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect_err("a file that is not a package is not a preset");
    assert!(matches!(error, PresetProblem::Unreadable { .. }), "{error}");

    let mut corrupt = good.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    let damaged = directory.path().join("damaged.zupui");
    std::fs::write(&damaged, &corrupt).expect("written");
    let error = select(
        &damaged,
        &installer,
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect_err("a package whose binary does not verify is not composable");
    assert!(matches!(error, PresetProblem::Damaged { .. }), "{error}");
}

/// A target the package does not carry is a refusal that names both what was
/// asked for and what is there, because "unsupported target" alone leaves the
/// author guessing.
#[test]
fn a_target_the_package_does_not_carry_names_what_it_has() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST, "aarch64-apple-darwin"]);
    let error = select(
        &path,
        &installer(),
        &target("x86_64-unknown-linux-gnu"),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect_err("this package has no Linux binary");
    let message = error.to_string();
    assert!(message.contains("x86_64-unknown-linux-gnu"), "{message}");
    assert!(message.contains(HOST), "{message}");
    assert!(message.contains("aarch64-apple-darwin"), "{message}");
}

/// A preset built against another wire protocol is its own refusal, distinct from
/// a missing capability, because adding a capability would not have helped.
#[test]
fn a_preset_from_another_protocol_generation_is_refused_as_such() {
    let directory = tempfile::tempdir().expect("a directory");
    let mut described = description();
    described.ui_protocol = zup_preset_protocol::PRESET_PROTOCOL_VERSION + 1;
    let mut writer = PresetPackageWriter::new(described).expect("a description");
    writer
        .add_binary(target(HOST), b"a preset".to_vec())
        .expect("a binary");
    let path = directory.path().join("other-generation.zupui");
    std::fs::write(&path, writer.finish().expect("a package")).expect("written");

    let error = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect_err("this package speaks another protocol generation");
    let message = error.to_string();
    assert!(message.contains("preset protocol"), "{message}");
    assert!(
        !message.contains("capabilit"),
        "a different axis: {message}"
    );
}

/// A capability this application cannot provide is named, not collapsed into a
/// generic refusal. An application with no components cannot show a preset that
/// requires them.
#[test]
fn a_capability_this_application_cannot_provide_is_named() {
    let directory = tempfile::tempdir().expect("a directory");
    let needy = PresetDescription::new("needy", "1.0.0", schema())
        .with_capabilities(Capabilities::new([Capability::Maintenance]));
    let mut writer = PresetPackageWriter::new(needy).expect("a description");
    writer
        .add_binary(target(HOST), b"a preset".to_vec())
        .expect("a binary");
    let path = directory.path().join("needy.zupui");
    std::fs::write(&path, writer.finish().expect("a package")).expect("written");

    // The application does offer maintenance, so this one is presentable. The
    // refusal below is the mirror: an application that configures no updates
    // cannot present a preset that needs them.
    let error = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    );
    assert!(
        error.is_ok(),
        "maintenance is something this host can provide: {error:?}"
    );

    let updater = PresetDescription::new("updater", "1.0.0", schema())
        .with_capabilities(Capabilities::new([Capability::Updates]));
    let mut writer = PresetPackageWriter::new(updater).expect("a description");
    writer
        .add_binary(target(HOST), b"a preset".to_vec())
        .expect("a binary");
    let path = directory.path().join("updater.zupui");
    std::fs::write(&path, writer.finish().expect("a package")).expect("written");
    let error = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect_err("this application configures no updates");
    let message = error.to_string();
    assert!(message.contains("updates"), "{message}");
    assert!(
        !message.contains("preset protocol"),
        "a different axis: {message}"
    );
}

/// Settings the preset's own schema rejects are refused, and the refusal points
/// at the setting the author wrote rather than at validator internals.
#[test]
fn settings_the_schema_rejects_are_named_where_the_author_wrote_them() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);

    let error = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": 42 })),
    )
    .expect_err("an accent is a string");
    let message = error.to_string();
    assert!(message.contains("ui.settings.accent"), "{message}");

    let error = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "hero": "x" })),
    )
    .expect_err("accent is required");
    assert!(error.to_string().contains("ui.settings"), "{error}");
}

/// A preset decides whether an undeclared setting is a mistake or a spare.
///
/// Its schema is generated from its own `Settings` type, and a type that
/// tolerates unknown fields generates a schema that does too - so a build
/// forwards what a preset is prepared to receive. A preset whose `Settings`
/// refuses unknown fields generates `additionalProperties: false`, and then the
/// same misspelling is the author's error, reported against the name they wrote
/// rather than silently dropped.
#[test]
fn an_undeclared_setting_follows_the_presets_own_schema() {
    let directory = tempfile::tempdir().expect("a directory");
    let mut strict = schema();
    strict["additionalProperties"] = serde_json::json!(false);
    let mut writer = PresetPackageWriter::new(
        PresetDescription::new("aurora", "1.4.2", strict)
            .with_capabilities(Capabilities::new([Capability::Components])),
    )
    .expect("a description");
    writer
        .add_binary(target(HOST), b"a preset".to_vec())
        .expect("a binary");
    let strict_path = directory.path().join("strict.zupui");
    std::fs::write(&strict_path, writer.finish().expect("a package")).expect("written");

    let value = settings(serde_json::json!({ "accent": "#695cff", "acolours": "#000000" }));

    let lenient = select(
        &write_package(directory.path(), &[HOST]),
        &installer(),
        &target(HOST),
        &value,
    )
    .expect("a preset whose settings type tolerates extras is given them");
    assert_eq!(
        lenient.settings.get("acolours").and_then(|v| v.as_str()),
        Some("#000000"),
        "the value reaches the preset rather than being dropped on the way"
    );

    let error = select(&strict_path, &installer(), &target(HOST), &value)
        .expect_err("a preset that refuses unknown fields gets none");
    assert!(
        error.to_string().contains("ui.settings.acolours"),
        "{error}"
    );
}

/// A schema that reaches outside itself is refused rather than resolved. An
/// application must not be able to make a build read a file off the machine or
/// make a request over a network by writing a `$ref`.
#[rstest::rstest]
#[case::https("https://example.test/schema.json")]
#[case::http("http://example.test/schema.json")]
#[case::file("file:///etc/passwd")]
fn a_schema_that_reaches_outside_itself_is_refused(#[case] reference: &str) {
    let mut remote = schema();
    remote["properties"]["accent"]["$ref"] = serde_json::json!(reference);
    let problems = validate_settings(&remote, &serde_json::json!({ "accent": "#695cff" }));
    assert_eq!(problems.len(), 1, "one refusal, not a fetch: {problems:?}");
    assert!(
        problems[0].message.contains("self-contained"),
        "the refusal says why: {}",
        problems[0].message
    );
    assert!(
        problems[0].message.contains(reference),
        "and names what it would have reached for: {}",
        problems[0].message
    );
}

/// A local `$ref` is how a generated schema marks an asset, so it must resolve.
#[test]
fn a_schema_that_refers_only_to_itself_is_accepted() {
    assert!(
        validate_settings(&schema(), &serde_json::json!({ "accent": "#695cff" })).is_empty(),
        "a self-contained schema is the normal case"
    );
}

/// The asset marker is what tells a build which settings are files, and it has to
/// survive the shape a generated schema actually produces.
#[test]
fn the_asset_settings_are_read_from_the_packages_own_schema() {
    assert_eq!(asset_settings(&schema()), ["logo"]);
    assert_eq!(
        asset_settings(&serde_json::json!({ "type": "object" })),
        Vec::<String>::new(),
        "a preset with no asset settings declares none"
    );
    assert_eq!(
        asset_settings(&serde_json::json!({
            "type": "object",
            "properties": { "plain": { "type": "string" } }
        })),
        Vec::<String>::new(),
        "a string is a string"
    );
}

/// An application-provided asset is resolved, hashed, and recorded by content.
/// Identical bytes under two names are one stored blob.
#[test]
fn an_application_asset_is_resolved_hashed_and_deduplicated() {
    use zup_preset_compose::prepare;

    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    std::fs::create_dir_all(directory.path().join("branding")).expect("a directory");
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>";
    std::fs::write(directory.path().join("branding/logo.svg"), svg).expect("the asset");

    let selected = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff", "logo": "branding/logo.svg" })),
    )
    .expect("the settings fit");
    let prepared = prepare(directory.path(), &selected, &PortableSourceFilePolicy)
        .expect("the asset resolves inside the project");

    assert_eq!(prepared.assets.len(), 1);
    let asset = &prepared.assets[0];
    assert_eq!(asset.name.as_str(), "logo");
    assert_eq!(asset.size as usize, svg.len());
    assert_eq!(
        asset.sha256,
        zup_core::Sha256Digest::from_bytes(sha2::Sha256::digest(svg).into())
    );
    assert_eq!(
        prepared.runtime.assets[0].sha256, asset.sha256,
        "what the installer carries is the same content the build read"
    );
    assert_eq!(
        prepared.runtime.settings["logo"], "branding/logo.svg",
        "the preset is told the name the application wrote, not a path"
    );
}

/// A path that leaves the project is refused where it is written, before any
/// file system access happens at all.
#[rstest::rstest]
#[case::parent("../outside.svg")]
#[case::embedded_parent("branding/../../outside.svg")]
#[case::rooted("/etc/passwd")]
#[case::drive(r"C:\Windows\System32\x.dll")]
fn an_asset_path_that_leaves_the_project_is_refused_while_reading_the_settings(
    #[case] source: &str,
) {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    let error = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff", "logo": source })),
    )
    .expect_err("this path names a machine, not a project");
    assert!(
        matches!(error, PresetProblem::AssetRejected { .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains("logo"),
        "and names the setting: {error}"
    );
}

/// A path inside the project that names nothing is a build failure, not a runtime
/// surprise: a window that opens without the logo it was configured with is
/// worse than a build that stops.
#[test]
fn an_asset_that_is_not_there_is_refused_before_anything_is_composed() {
    use zup_preset_compose::prepare;

    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    let selected = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff", "logo": "branding/absent.svg" })),
    )
    .expect("the path is a project path");
    let error = prepare(directory.path(), &selected, &PortableSourceFilePolicy)
        .expect_err("there is no such file to read");
    assert!(
        matches!(error, PresetProblem::AssetRejected { .. }),
        "{error}"
    );
}

/// The file an asset names has to be a file. A directory named by a setting is a
/// mistake that would otherwise become an unreadable file beside the runtime.
#[test]
fn an_asset_that_is_a_directory_is_refused() {
    use zup_preset_compose::prepare;

    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    std::fs::create_dir_all(directory.path().join("branding/logo.svg")).expect("a directory");
    let selected = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff", "logo": "branding/logo.svg" })),
    )
    .expect("the path is a project path");
    prepare(directory.path(), &selected, &PortableSourceFilePolicy)
        .expect_err("a directory is not a logo");
}

/// An asset over the build's limit is refused, and the limit is stated.
#[test]
fn an_asset_over_the_size_limit_is_refused() {
    use zup_preset_compose::{MAX_ASSET_BYTES, prepare};

    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    // One byte past the limit, so the refusal is about the bound and not about
    // a file that is somehow enormous.
    let oversized = vec![b'x'; (MAX_ASSET_BYTES + 1) as usize];
    std::fs::write(directory.path().join("huge.bin"), &oversized).expect("the asset");

    let selected = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff", "logo": "huge.bin" })),
    )
    .expect("the path is a project path");
    let error = prepare(directory.path(), &selected, &PortableSourceFilePolicy)
        .expect_err("an asset this large is not one a preset draws with");
    let message = error.to_string();
    assert!(message.contains("preset asset"), "{message}");
}

/// Changing the asset's bytes changes its digest, because identity is content.
#[test]
fn changed_asset_bytes_change_the_digest() {
    use zup_preset_compose::prepare;

    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    let asset_path = directory.path().join("logo.svg");

    let digest_of = |bytes: &[u8]| {
        std::fs::write(&asset_path, bytes).expect("the asset");
        let selected = select(
            &path,
            &installer(),
            &target(HOST),
            &settings(serde_json::json!({ "accent": "#695cff", "logo": "logo.svg" })),
        )
        .expect("the settings fit");
        prepare(directory.path(), &selected, &PortableSourceFilePolicy)
            .expect("the asset resolves")
            .assets[0]
            .sha256
    };

    let before = digest_of(b"<svg/>");
    let same = digest_of(b"<svg/>");
    let after = digest_of(b"<svg width='8'/>");
    assert_eq!(before, same, "the same bytes are the same asset");
    assert_ne!(before, after, "different bytes are a different asset");
}

/// An application that configures nothing is a normal application, as long as
/// the preset it chose requires nothing either.
#[test]
fn an_application_that_configures_nothing_against_a_preset_that_needs_nothing() {
    let directory = tempfile::tempdir().expect("a directory");
    let mut writer = PresetPackageWriter::new(
        PresetDescription::new("bare", "1.0.0", serde_json::json!({ "type": "object" }))
            .with_capabilities(Capabilities::new([Capability::Components])),
    )
    .expect("a description");
    writer
        .add_binary(target(HOST), b"a preset".to_vec())
        .expect("a binary");
    let path = directory.path().join("bare.zupui");
    std::fs::write(&path, writer.finish().expect("a package")).expect("written");

    let selected = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({})),
    )
    .expect("a preset that requires no settings is satisfied by an application that sets none");
    assert!(selected.asset_settings.is_empty());
    assert!(
        selected
            .settings
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
}

/// The same bytes through the same path produce the same selection, so a build
/// is reproducible.
#[test]
fn selecting_the_same_package_twice_gives_the_same_thing() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST, "aarch64-apple-darwin"]);
    let value = settings(serde_json::json!({ "accent": "#695cff" }));
    let first = select(&path, &installer(), &target(HOST), &value).expect("selected");
    let second = select(&path, &installer(), &target(HOST), &value).expect("selected");
    assert_eq!(first.executable, second.executable);
    assert_eq!(first.version, second.version);
    assert_eq!(first.required_capabilities, second.required_capabilities);
}

/// The package a build consumes is the package `zup preset inspect` reads, which is
/// what makes "verify before composing" the same statement in both commands.
#[test]
fn a_build_and_an_inspector_read_one_package() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST, "aarch64-apple-darwin"]);
    let view = PresetPackageView::open(std::fs::read(&path).expect("read")).expect("opens");
    view.verify().expect("verifies");

    let selected = select(
        &path,
        &installer(),
        &target("aarch64-apple-darwin"),
        &settings(serde_json::json!({ "accent": "#695cff" })),
    )
    .expect("selected");
    assert_eq!(selected.name, view.name());
    assert_eq!(selected.version, *view.version());
    assert_eq!(selected.protocol, view.wire_protocol());
    assert_eq!(
        selected.required_capabilities,
        *view.required_capabilities()
    );
}

/// What the installer carries is what a runtime needs and nothing more.
///
/// The settings are the application's own values, the assets are named and
/// digested rather than sourced, and the package this came from is not recorded -
/// a runtime has no use for a build-machine fact, and recording one would be how
/// a source path eventually reached a user's machine.
#[test]
fn the_runtime_model_names_assets_without_where_they_live() {
    use zup_preset_compose::prepare;

    let directory = tempfile::tempdir().expect("a directory");
    let path = write_package(directory.path(), &[HOST]);
    std::fs::create_dir_all(directory.path().join("branding")).expect("a directory");
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>";
    std::fs::write(directory.path().join("branding/logo.svg"), svg).expect("the asset");

    let selected = select(
        &path,
        &installer(),
        &target(HOST),
        &settings(serde_json::json!({ "accent": "#695cff", "logo": "./branding/logo.svg" })),
    )
    .expect("the settings fit");
    let prepared = prepare(directory.path(), &selected, &PortableSourceFilePolicy)
        .expect("the asset resolves inside the project");

    assert_eq!(prepared.runtime.name.as_str(), "aurora");
    assert_eq!(
        prepared.runtime.settings["accent"], "#695cff",
        "the application's own value, verbatim"
    );
    assert_eq!(
        prepared.runtime.settings["logo"], "./branding/logo.svg",
        "the preset is told exactly what the author wrote; normalising it would be the \
         host guessing at the author's spelling"
    );
    assert_eq!(prepared.runtime.assets.len(), 1);
    assert_eq!(prepared.runtime.assets[0].name.as_str(), "logo");
    assert_eq!(prepared.runtime.assets[0].size as usize, svg.len());

    // The path the bytes were read from is the build's fact, and the runtime has
    // no use for it: the asset record is a name and a digest, and the whole model
    // records nothing about the machine that composed it.
    let assets = serde_json::to_string(&prepared.runtime.assets).expect("the assets encode");
    assert!(
        !assets.contains("branding") && !assets.contains("svg"),
        "an asset is named, not sourced: {assets}"
    );
    let whole = serde_json::to_string(&prepared.runtime).expect("the runtime model encodes");
    assert!(
        !whole.contains(&directory.path().to_string_lossy().into_owned()),
        "the runtime model carries no build-machine path: {whole}"
    );
    assert!(
        !whole.contains("aurora.zupui"),
        "and no record of where the package came from: {whole}"
    );
    assert!(
        prepared.assets[0].source.is_some(),
        "the source is a build-machine fact the build keeps and the runtime does not see"
    );
}
