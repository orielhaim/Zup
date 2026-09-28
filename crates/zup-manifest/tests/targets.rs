use std::path::PathBuf;

use miette::Diagnostic;
use rstest::rstest;
use zup_manifest::{
    Frontend, InstallScope, Installer, Manifest, ManifestError, Source, TargetOverrideSet,
    TargetOverrides, TargetProfileId, TargetTriple, Template, compile, parse, parse_and_compile,
    select_targets, select_targets_with,
};

const MANIFEST: &str = r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.z-linux-arm64]
target = "aarch64-unknown-linux-gnu"
source = { directory = "dist/linux-arm64" }

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[build.targets.a-macos-arm64]
target = "aarch64-apple-darwin"
source = { directory = "dist/macos-arm64" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"
"#;

fn with_windows_frontend(frontend: &str) -> String {
    MANIFEST.replace(
        r#"source = { directory = "dist/windows-x64" }"#,
        &format!("source = {{ directory = \"dist/windows-x64\" }}\nfrontend = \"{frontend}\""),
    )
}

fn with_windows_install() -> String {
    MANIFEST.replace(
        r#"source = { directory = "dist/windows-x64" }"#,
        r#"source = { directory = "dist/windows-x64" }

[build.targets.windows-x64.install]
scope = "machine"
allow_directory_override = true

[build.targets.windows-x64.install.directory]
machine = "${location.programs}/WindowsAcme""#,
    )
}

fn with_resources(resources: &str) -> String {
    format!("{MANIFEST}\n{}", resources.trim_start())
}

fn compile_profile(source: &str, profile: &str) -> Result<Installer, ManifestError> {
    compile_parsed(&parse(source).unwrap(), profile)
}

fn compile_parsed(manifest: &Manifest, profile: &str) -> Result<Installer, ManifestError> {
    let overrides = TargetOverrides::default();
    let selected = select_targets(manifest, &[profile], &overrides).unwrap();
    compile(manifest, &selected[0], &overrides)
}

#[test]
fn selects_targets_by_profile_name() {
    let manifest = parse(MANIFEST).unwrap();

    let selected =
        select_targets(&manifest, &["windows-x64"], &TargetOverrides::default()).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].profile,
        TargetProfileId::new("windows-x64").unwrap()
    );
    assert_eq!(
        selected[0].target,
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    );
    assert_eq!(
        selected[0].source,
        Source::new(PathBuf::from("dist/windows-x64")).unwrap()
    );
    assert_eq!(selected[0].frontend, Frontend::Gui);

    // An empty selector list resolves every profile, in name order.
    let selected = select_targets(&manifest, &[], &TargetOverrides::default()).unwrap();
    let names: Vec<_> = selected
        .iter()
        .map(|config| config.profile.as_str())
        .collect();
    assert_eq!(names, ["a-macos-arm64", "windows-x64", "z-linux-arm64"]);
}

#[test]
fn a_selector_may_be_a_profile_name_or_a_raw_triple() {
    // A raw triple resolves to the profile that declares it.
    let manifest = parse(MANIFEST).unwrap();
    let selected = select_targets(
        &manifest,
        &["x86_64-pc-windows-msvc"],
        &TargetOverrides::default(),
    )
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].profile.as_str(), "windows-x64");

    // When a profile is *named* after a triple another profile also declares,
    // the profile name wins over the triple lookup.
    let source = MANIFEST.replace(
        "[build.targets.z-linux-arm64]",
        r#"[build.targets."x86_64-pc-windows-msvc"]
target = "x86_64-unknown-linux-gnu"
source = { directory = "dist/linux-x64" }

[build.targets.z-linux-arm64]"#,
    );
    let manifest = parse(&source).unwrap();
    let selected = select_targets(
        &manifest,
        &["x86_64-pc-windows-msvc"],
        &TargetOverrides::default(),
    )
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].profile.as_str(), "x86_64-pc-windows-msvc");
    assert_eq!(selected[0].target.as_str(), "x86_64-unknown-linux-gnu");
}

#[test]
fn raw_target_aliases_are_canonicalized() {
    let source = MANIFEST.replace("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc");
    let manifest = parse(&source).unwrap();

    let selected = select_targets(
        &manifest,
        &["arm64-pc-windows-msvc"],
        &TargetOverrides::default(),
    )
    .unwrap();

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].target.as_str(), "aarch64-pc-windows-msvc");
}

#[test]
fn declared_frontend_resolves_and_compiles() {
    // A `frontend` on the common manifest and a `frontend` on a target profile
    // are the same claim: the innermost declaration reaches the installer.
    let profile = with_windows_frontend("console");
    let common = MANIFEST.replace("schema = 1\n", "schema = 1\nfrontend = \"console\"\n");

    for source in [profile, common] {
        let manifest = parse(&source).unwrap();
        let overrides = TargetOverrides::default();

        let selected = select_targets(&manifest, &["windows-x64"], &overrides).unwrap();
        let installer = compile(&manifest, &selected[0], &overrides).unwrap();
        let convenience = parse_and_compile(&source, "windows-x64").unwrap();

        assert_eq!(selected[0].frontend, Frontend::Console);
        assert_eq!(installer.frontend, Frontend::Console);
        assert_eq!(convenience.frontend, Frontend::Console);
    }
}

#[test]
fn caller_frontend_override_wins_over_profile() {
    let manifest = parse(&with_windows_frontend("console")).unwrap();
    let overrides = TargetOverrides {
        frontend: Some(Frontend::Headless),
        ..TargetOverrides::default()
    };

    let selected = select_targets(&manifest, &["windows-x64"], &overrides).unwrap();
    let installer = compile(&manifest, &selected[0], &overrides).unwrap();

    assert_eq!(selected[0].frontend, Frontend::Headless);
    assert_eq!(installer.frontend, Frontend::Headless);
    assert!(matches!(
        compile(&manifest, &selected[0], &TargetOverrides::default()),
        Err(ManifestError::InvalidResolvedTargetConfig { .. })
    ));
}

#[test]
fn empty_and_missing_target_lists_apply_to_all_profiles() {
    let source = with_resources(
        r#"
[[files]]
source = "missing/**/*"
destination = "${install}"

[[files]]
source = "empty/**/*"
destination = "${install}"
targets = []
"#,
    );

    let windows = compile_profile(&source, "windows-x64").unwrap();
    let linux = compile_profile(&source, "z-linux-arm64").unwrap();

    assert_eq!(windows.files.len(), 2);
    assert_eq!(linux.files.len(), 2);
}

#[test]
fn target_filters_windows_and_linux_files() {
    let source = with_resources(
        r#"
[[files]]
source = "windows/**/*"
destination = "${install}"
targets = ["windows-x64"]

[[files]]
source = "linux/**/*"
destination = "${install}"
targets = ["z-linux-arm64"]
"#,
    );

    let windows = compile_profile(&source, "windows-x64").unwrap();
    let linux = compile_profile(&source, "z-linux-arm64").unwrap();

    assert_eq!(windows.files.len(), 1);
    assert_eq!(windows.files[0].source.as_str(), "windows/**/*");
    assert_eq!(linux.files.len(), 1);
    assert_eq!(linux.files[0].source.as_str(), "linux/**/*");
}

#[test]
fn component_references_are_validated_per_target() {
    let source = with_resources(
        r#"
[[components]]
id = "windows-only"
name = "Windows only"
targets = ["windows-x64"]

[[files]]
source = "**/*"
destination = "${install}"
component = "windows-only"
"#,
    );

    let windows = compile_profile(&source, "windows-x64").unwrap();
    let linux_error = compile_profile(&source, "z-linux-arm64").unwrap_err();

    assert!(
        windows
            .components
            .iter()
            .any(|component| component.id.as_str() == "windows-only")
    );
    assert!(matches!(
        linux_error,
        ManifestError::UnknownComponent { ref id, .. } if id == "windows-only"
    ));
}

#[test]
fn target_filters_every_resource_kind() {
    let source = with_resources(
        r#"
[[launchers]]
location = "desktop"
name = "Acme"
target = "${install}/Acme.exe"
targets = ["windows-x64"]

[[path]]
value = "${install}/bin"
targets = ["z-linux-arm64"]

[[services]]
id = "windows-service"
name = "Windows Service"
binary = "${install}/service.exe"
start = "manual"
targets = ["windows-x64"]

[[protocols]]
scheme = "acme"
executable = "${install}/Acme.exe"
targets = ["z-linux-arm64"]

[[file_associations]]
extension = ".acme"
id = "Acme.Document"
executable = "${install}/Acme.exe"
targets = ["windows-x64"]

[[prerequisites]]
id = "windows-runtime"
name = "Windows Runtime"
targets = ["windows-x64"]
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime.exe", sha256 = "0000000000000000000000000000000000000000000000000000000000000000", size = 1 }

[[plugins]]
id = "windows-plugin"
source = "plugins/windows.wasm"
targets = ["windows-x64"]
"#,
    );

    let windows = compile_profile(&source, "windows-x64").unwrap();
    let linux = compile_profile(&source, "z-linux-arm64").unwrap();

    assert_eq!(windows.launchers.len(), 1);
    assert!(windows.path.is_empty());
    assert_eq!(windows.services.len(), 1);
    assert!(windows.protocols.is_empty());
    assert_eq!(windows.file_associations.len(), 1);
    assert_eq!(windows.prerequisites.len(), 1);
    assert_eq!(windows.prerequisites[0].id.as_str(), "windows-runtime");
    assert_eq!(windows.plugins.len(), 1);
    assert_eq!(windows.plugins[0].id.as_str(), "windows-plugin");

    assert!(linux.launchers.is_empty());
    assert_eq!(linux.path.len(), 1);
    assert!(linux.services.is_empty());
    assert_eq!(linux.protocols.len(), 1);
    assert!(linux.file_associations.is_empty());
    assert!(linux.prerequisites.is_empty());
    assert!(linux.plugins.is_empty());
}

#[test]
fn duplicate_resources_are_rejected_only_when_they_share_a_target() {
    let source = with_resources(
        r#"
[[components]]
id = "duplicate"
name = "Windows"
targets = ["windows-x64"]

[[components]]
id = "duplicate"
name = "Windows duplicate"
targets = ["windows-x64"]

[[components]]
id = "duplicate"
name = "Linux"
targets = ["z-linux-arm64"]
"#,
    );

    assert!(matches!(
        compile_profile(&source, "windows-x64"),
        Err(ManifestError::DuplicateComponent { ref id, .. }) if id == "duplicate"
    ));
    assert!(compile_profile(&source, "z-linux-arm64").is_ok());
}

#[rstest]
#[case::component(
    "component",
    r#"
[[components]]
id = "core"
name = "Core"
targets = ["missing"]
"#
)]
#[case::prerequisite("prerequisite", r#"
[[prerequisites]]
id = "runtime"
name = "Runtime"
targets = ["missing"]
requirement = { kind = "runtime", id = "windows.vc.v14" }
package = { type = "embedded", path = "runtime.exe", sha256 = "0000000000000000000000000000000000000000000000000000000000000000", size = 1 }
"#)]
#[case::plugin(
    "plugin",
    r#"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
targets = ["missing"]
"#
)]
#[case::launcher(
    "launcher",
    r#"
[[launchers]]
location = "desktop"
name = "Acme"
target = "${install}/Acme.exe"
targets = ["missing"]
"#
)]
#[case::path_entry(
    "path entry",
    r#"
[[path]]
value = "${install}/bin"
targets = ["missing"]
"#
)]
#[case::service(
    "service",
    r#"
[[services]]
id = "agent"
name = "Agent"
binary = "${install}/service.exe"
start = "manual"
targets = ["missing"]
"#
)]
#[case::protocol(
    "protocol",
    r#"
[[protocols]]
scheme = "acme"
executable = "${install}/Acme.exe"
targets = ["missing"]
"#
)]
#[case::file_association(
    "file association",
    r#"
[[file_associations]]
extension = ".acme"
id = "Acme.Document"
executable = "${install}/Acme.exe"
targets = ["missing"]
"#
)]
fn every_resource_kind_names_itself_in_the_diagnostic(#[case] kind: &str, #[case] table: &str) {
    let source = with_resources(table);

    let error = parse(&source).unwrap_err();

    assert!(
        matches!(
            error,
            ManifestError::UnknownTargetProfileReference {
                resource,
                ref profile,
                ..
            } if resource.as_str() == kind && profile == "missing"
        ),
        "kind: {kind}, error: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("{kind} references unknown target profile `missing`")
    );
}

#[test]
fn invalid_plugin_source_is_an_authoring_error_for_every_profile() {
    // Parse checks the source of every plugin, not only the plugins the profile
    // being compiled would select, so a bad source fails regardless of targets.
    let source = with_resources(
        r#"
[[plugins]]
id = "linux-only"
source = "../helper.wasm"
targets = ["z-linux-arm64"]
"#,
    );

    assert!(
        matches!(
            parse(&source),
            Err(ManifestError::InvalidPluginSource { ref path, .. }) if path == "../helper.wasm"
        ),
        "{:?}",
        parse(&source)
    );
}

#[test]
fn compile_only_checks_the_source_of_a_plugin_the_profile_selects() {
    let source = with_resources(
        r#"
[[plugins]]
id = "linux-only"
source = "plugins/helper.wasm"
targets = ["z-linux-arm64"]
"#,
    );
    let mut manifest = parse(&source).unwrap();
    // A source that only parse could have caught, for a plugin the Windows
    // profile never sees.
    manifest.plugins[0].value.source = "../helper.wasm".to_owned();

    let windows = compile_parsed(&manifest, "windows-x64").unwrap();
    assert!(windows.plugins.is_empty());
    assert!(matches!(
        compile_parsed(&manifest, "z-linux-arm64"),
        Err(ManifestError::InvalidPluginSource { ref path, .. }) if path == "../helper.wasm"
    ));
}

#[test]
fn rejects_empty_target_matrix_at_parse_and_selection() {
    let source = MANIFEST.replace(
        r#"[build]

[build.targets.z-linux-arm64]
target = "aarch64-unknown-linux-gnu"
source = { directory = "dist/linux-arm64" }

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[build.targets.a-macos-arm64]
target = "aarch64-apple-darwin"
source = { directory = "dist/macos-arm64" }"#,
        "[build]\n[build.targets]",
    );

    let error = parse(&source).unwrap_err();
    assert_eq!(
        error.code().map(|code| code.to_string()),
        Some("zup_manifest::empty_target_matrix".to_owned())
    );

    let mut manifest = parse(MANIFEST).unwrap();
    manifest.build.targets.clear();
    let error = select_targets(&manifest, &[], &TargetOverrides::default()).unwrap_err();
    assert!(matches!(error, ManifestError::EmptyTargetMatrix { .. }));
}

#[test]
fn profile_install_replaces_common_install_in_resolution_and_compilation() {
    let source = with_windows_install();
    let manifest = parse(&source).unwrap();
    let overrides = TargetOverrides::default();
    let windows = select_targets(&manifest, &["windows-x64"], &overrides).unwrap();
    let linux = select_targets(&manifest, &["z-linux-arm64"], &overrides).unwrap();

    assert_eq!(windows[0].install.scope, InstallScope::Machine);
    assert!(windows[0].install.allow_directory_override);
    assert_eq!(linux[0].install.scope, InstallScope::User);
    assert!(!linux[0].install.allow_directory_override);

    let installer = compile(&manifest, &windows[0], &overrides).unwrap();
    assert_eq!(installer.install, windows[0].install);
}

fn literal(value: &str) -> Template {
    Template::parse(value).unwrap()
}

#[test]
fn caller_source_override_wins_over_profile_and_common_manifest() {
    let manifest = parse(MANIFEST).unwrap();
    let overrides = TargetOverrides {
        source: Some(Source::new(PathBuf::from("out/cli")).unwrap()),
        ..TargetOverrides::default()
    };

    let selected = select_targets(&manifest, &["windows-x64"], &overrides).unwrap();
    let installer = compile(&manifest, &selected[0], &overrides).unwrap();

    assert_eq!(
        selected[0].source.directory,
        PathBuf::from("out/cli"),
        "the CLI source replaces the profile declaration"
    );
    assert!(
        !installer
            .files
            .iter()
            .any(|file| file.source == "dist/windows-x64"),
        "compilation follows the resolved source, not the declaration"
    );
    assert!(matches!(
        compile(&manifest, &selected[0], &TargetOverrides::default()),
        Err(ManifestError::InvalidResolvedTargetConfig { ref reason, .. })
            if reason.contains("source")
    ));
}

#[test]
fn caller_install_directory_override_wins_over_profile_and_common_manifest() {
    let manifest = parse(&with_windows_install()).unwrap();
    let overrides = TargetOverrides {
        install_directory: Some(literal("D:/Apps/Acme")),
        ..TargetOverrides::default()
    };

    let selected = select_targets(&manifest, &["windows-x64"], &overrides).unwrap();
    let installer = compile(&manifest, &selected[0], &overrides).unwrap();

    // The target installs per machine, so only the machine template is replaced.
    assert_eq!(
        selected[0]
            .install
            .directory
            .machine
            .as_ref()
            .map(ToString::to_string),
        Some("D:/Apps/Acme".to_owned())
    );
    assert!(selected[0].install.directory.user.is_none());
    // The override replaces a destination, not the scope or the policy.
    assert_eq!(selected[0].install.scope, InstallScope::Machine);
    assert!(selected[0].install.allow_directory_override);
    assert_eq!(installer.install, selected[0].install);
    assert!(matches!(
        compile(&manifest, &selected[0], &TargetOverrides::default()),
        Err(ManifestError::InvalidResolvedTargetConfig { ref reason, .. })
            if reason.contains("install directory")
    ));
}

#[test]
fn caller_install_directory_override_covers_every_scope_a_target_installs_to() {
    let source = MANIFEST.replace("scope = \"user\"\n", "scope = \"either\"\n");
    let manifest = parse(&source).unwrap();
    let overrides = TargetOverrides {
        install_directory: Some(literal("C:/Program Files/Acme")),
        ..TargetOverrides::default()
    };

    let selected = select_targets(&manifest, &["windows-x64"], &overrides).unwrap();
    let installer = compile(&manifest, &selected[0], &overrides).unwrap();

    assert_eq!(selected[0].install.scope, InstallScope::Either);
    for template in [
        &selected[0].install.directory.user,
        &selected[0].install.directory.machine,
    ] {
        assert_eq!(
            template.as_ref().map(ToString::to_string),
            Some("C:/Program Files/Acme".to_owned())
        );
    }
    assert_eq!(installer.install.directory, selected[0].install.directory);
}

#[test]
fn an_override_set_resolves_each_profile_independently() {
    let manifest = parse(MANIFEST).unwrap();
    let mut overrides = TargetOverrideSet::default();
    overrides.apply(
        TargetProfileId::new("windows-x64").unwrap(),
        TargetOverrides {
            source: Some(Source::new(PathBuf::from("out/windows")).unwrap()),
            install_directory: Some(literal("C:/Acme")),
            frontend: Some(Frontend::Console),
        },
    );
    overrides.apply(
        TargetProfileId::new("z-linux-arm64").unwrap(),
        TargetOverrides {
            source: Some(Source::new(PathBuf::from("out/linux")).unwrap()),
            ..TargetOverrides::default()
        },
    );

    let selected =
        select_targets_with(&manifest, &["windows-x64", "z-linux-arm64"], &overrides).unwrap();
    let windows = &selected[0];
    let linux = &selected[1];
    assert_eq!(windows.profile.as_str(), "windows-x64");
    assert_eq!(linux.profile.as_str(), "z-linux-arm64");
    assert_eq!(windows.source.directory, PathBuf::from("out/windows"));
    assert_eq!(windows.frontend, Frontend::Console);
    assert_eq!(
        windows
            .install
            .directory
            .user
            .as_ref()
            .map(ToString::to_string),
        Some("C:/Acme".to_owned())
    );
    assert_eq!(linux.source.directory, PathBuf::from("out/linux"));
    assert_eq!(
        linux.frontend,
        Frontend::Gui,
        "the common manifest still applies"
    );
    assert_eq!(
        linux
            .install
            .directory
            .user
            .as_ref()
            .map(ToString::to_string),
        Some("${location.user_data}/Acme".to_owned()),
        "a profile without an entry keeps the manifest value"
    );

    // A uniform set reaches every selected profile through the same resolution.
    let uniform = TargetOverrideSet::uniform(TargetOverrides {
        source: Some(Source::new(PathBuf::from("out/all")).unwrap()),
        ..TargetOverrides::default()
    });
    let selected = select_targets_with(&manifest, &[], &uniform).unwrap();
    assert!(
        selected
            .iter()
            .all(|config| config.source.directory == *"out/all"),
        "{selected:?}"
    );
}

#[test]
fn rejects_forged_resolved_target_config() {
    let manifest = parse(MANIFEST).unwrap();
    let overrides = TargetOverrides::default();
    let resolved = select_targets(&manifest, &["windows-x64"], &overrides).unwrap()[0].clone();

    let mut forged_profile = resolved.clone();
    forged_profile.profile = TargetProfileId::new("missing").unwrap();
    let mut forged_target = resolved.clone();
    forged_target.target = TargetTriple::parse("aarch64-apple-darwin").unwrap();
    let mut forged_source = resolved.clone();
    forged_source.source = Source::new(PathBuf::from("dist/forged")).unwrap();
    let mut forged_frontend = resolved.clone();
    forged_frontend.frontend = Frontend::Headless;
    let mut forged_install = resolved.clone();
    forged_install.install.scope = InstallScope::Machine;
    let mut forged_directory = resolved.clone();
    forged_directory.install.directory.user = Some(literal("C:/Forged"));
    let mut forged_policy = resolved.clone();
    forged_policy.install.allow_directory_override = true;

    for (config, field) in [
        (forged_profile, "profile"),
        (forged_target, "target"),
        (forged_source, "source"),
        (forged_frontend, "frontend"),
        (forged_install, "install scope"),
        (forged_directory, "install directory"),
        (forged_policy, "directory-override policy"),
    ] {
        let error = compile(&manifest, &config, &overrides).unwrap_err();
        assert_eq!(
            error.code().map(|code| code.to_string()),
            Some("zup_manifest::invalid_resolved_target_config".to_owned())
        );
        assert!(error.to_string().contains(field), "{error}");
    }

    // The same source and install directory are legitimate once declared.
    let declared = TargetOverrides {
        source: Some(Source::new(PathBuf::from("dist/forged")).unwrap()),
        install_directory: Some(literal("C:/Forged")),
        ..TargetOverrides::default()
    };
    let selected = select_targets(&manifest, &["windows-x64"], &declared).unwrap();
    let mut authentic = selected[0].clone();
    authentic.install.allow_directory_override = true;
    assert!(
        compile(&manifest, &authentic, &declared).is_err(),
        "a declared override does not legitimize other drift"
    );
    assert!(compile(&manifest, &selected[0], &declared).is_ok());
}

#[test]
fn rejects_unknown_selection() {
    let manifest = parse(MANIFEST).unwrap();

    let error = select_targets(&manifest, &["missing"], &TargetOverrides::default()).unwrap_err();

    assert_eq!(
        error.code().map(|code| code.to_string()),
        Some("zup_manifest::unknown_target_selector".to_owned())
    );
    assert!(error.help().unwrap().to_string().contains("windows-x64"));
    assert!(matches!(
        error,
        ManifestError::UnknownTargetSelector {
            ref selector,
            ref available,
            ..
        } if selector == "missing"
            && available.contains("windows-x64")
            && available.contains("linux-arm64")
    ));
}

#[test]
fn rejects_duplicate_canonical_targets() {
    let source = MANIFEST.replace(
        "[build.targets.windows-x64]",
        r#"[build.targets.zz-linux-alias]
target = "arm64-unknown-linux-gnu"
source = { directory = "dist/linux-alias" }

[build.targets.windows-x64]"#,
    );
    let manifest = parse(&source).unwrap();

    let error = select_targets(&manifest, &["windows-x64"], &TargetOverrides::default())
        .unwrap_err()
        .with_source(&source);

    assert_eq!(
        error.code().map(|code| code.to_string()),
        Some("zup_manifest::duplicate_target".to_owned())
    );
    assert!(error.help().is_some());
    assert!(matches!(
        error,
        ManifestError::DuplicateTarget {
            ref profile,
            ref conflicts_with,
            ref target,
            src: Some(_),
            span: Some(_),
        } if profile == "zz-linux-alias"
            && conflicts_with == "z-linux-arm64"
            && target == "aarch64-unknown-linux-gnu"
    ));
}

#[test]
fn installer_serialization_omits_build_directories() {
    let installer = parse_and_compile(MANIFEST, "windows-x64").unwrap();
    let json = serde_json::to_string(&installer).unwrap();
    assert!(!json.contains("dist/windows-x64"), "json: {json}");
}
