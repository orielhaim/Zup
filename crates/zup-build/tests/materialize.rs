use std::fs;
use std::path::Path;

use rstest::rstest;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{BuildError, FilePattern, Sha256Digest, Writes, materialize};
use zup_core::{MAX_PLUGIN_ARTIFACTS, TargetTriple};
use zup_manifest::{TargetOverrides, compile, parse, parse_and_compile, select_targets};

fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn project(files: &[(&str, &[u8])]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (rel, contents) in files {
        write_file(&dir.path().join(rel), contents);
    }
    dir
}

fn manifest_toml(files_block: &str) -> String {
    format!(
        r#"
schema = 1

[app]
id = "com.example.acme"
name = "Acme"
version = "1.0.0"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = {{ directory = "dist" }}

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/Acme"

{files_block}
"#
    )
}

fn materialize_one(
    manifest_path: &Path,
    manifest: &zup_manifest::Manifest,
    installer: zup_core::Installer,
) -> Result<zup_build::TargetBuildPlan, BuildError> {
    let config = select_targets(manifest, &["default"], &TargetOverrides::default())
        .expect("target")
        .into_iter()
        .next()
        .expect("selected target");
    let mut plan = materialize(
        manifest_path,
        manifest,
        vec![(config, installer)],
        Writes::None,
    )
    .map_err(unwrap_target_error)?;
    assert_eq!(plan.targets.len(), 1);
    Ok(plan.targets.pop().expect("target plan"))
}

fn unwrap_target_error(error: BuildError) -> BuildError {
    match error {
        BuildError::Target { source, .. } => *source,
        other => other,
    }
}

fn materialize_project(
    dir: &Path,
    files_block: &str,
) -> Result<zup_build::TargetBuildPlan, BuildError> {
    let source = manifest_toml(files_block);
    let manifest = parse(&source).expect("parse");
    let installer = parse_and_compile(&source, "default").expect("compile");
    materialize_one(&dir.join("zup.toml"), &manifest, installer)
}

#[test]
fn the_configured_update_root_bytes_are_embedded() {
    let dir = project(&[
        ("dist/app.exe", b"app"),
        ("keys/root.json", br#"{"signed":{"_type":"root"}}"#),
    ]);
    let source = format!(
        "{}\n[updates]\nrepository = \"https://updates.example.com/acme\"\nchannel = \"stable\"\nroot = \"keys/root.json\"\n",
        manifest_toml("")
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source, "default").unwrap();
    let plan = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap();
    let updates = plan.installer.updates.unwrap();
    assert_eq!(updates.repository, "https://updates.example.com/acme");
    assert_eq!(updates.channel, "stable");
    assert_eq!(updates.trusted_root, br#"{"signed":{"_type":"root"}}"#);
}

#[test]
fn embedded_prerequisite_is_materialized_with_exact_identity() {
    let bytes = b"runtime payload";
    let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    let dir = project(&[("dist/app.exe", b"app"), ("runtime.exe", bytes)]);
    let source = format!(
        r#"{}
[[prerequisites]]
id = "runtime"
name = "Runtime"
requirement = {{ kind = "runtime", id = "windows.vc.v14" }}
package = {{ type = "embedded", path = "runtime.exe", sha256 = "{digest}", size = {} }}
"#,
        manifest_toml(
            r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
        ),
        bytes.len()
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source, "default").unwrap();
    let plan = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap();
    assert_eq!(plan.prerequisites.len(), 1);
    assert_eq!(plan.prerequisites[0].sha256, digest);
    assert_eq!(plan.prerequisites[0].size, bytes.len() as u64);
    assert_eq!(plan.prerequisite_size, bytes.len() as u64);
}

#[test]
fn missing_source_root() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("zup.toml"), b"").unwrap();
    let err = materialize_project(dir.path(), "").unwrap_err();
    assert!(matches!(err, BuildError::SourceMissing { .. }), "{err:?}");
}

#[test]
fn a_source_root_that_is_not_a_directory_is_reported_as_such() {
    let dir = project(&[("dist", b"file-not-dir")]);
    let err = materialize_project(dir.path(), "").unwrap_err();
    assert!(
        matches!(err, BuildError::SourceNotDirectory { .. }),
        "{err:?}"
    );
}

#[test]
fn source_lexical_escape_rejected() {
    let dir = project(&[("dist/a.txt", b"a")]);
    let source = manifest_toml("").replace(r#"directory = "dist""#, r#"directory = "../outside""#);
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let err = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(
        matches!(err, BuildError::SourceEscapesProject { .. }),
        "{err:?}"
    );
}

#[test]
fn a_glob_with_a_static_root_does_not_copy_the_root_into_the_destination() {
    let dir = project(&[
        ("dist/bin/acme.exe", b"exe"),
        ("dist/bin/helpers/foo.dll", b"dll"),
        ("dist/other/skip.txt", b"skip"),
    ]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "bin/**/*"
destination = "${install}/tools"
"#,
    )
    .unwrap();

    let mut dests: Vec<_> = plan
        .files
        .iter()
        .map(|f| f.destination.to_string())
        .collect();
    dests.sort();
    assert_eq!(
        dests,
        [
            "${install}/tools/acme.exe".to_owned(),
            "${install}/tools/helpers/foo.dll".to_owned(),
        ]
    );
}

#[rstest]
#[case::one_segment(&["a.exe"], "*.exe")]
#[case::crossing_segments(&["a.exe", "nested/c.exe"], "**/*.exe")]
fn a_glob_matches_the_depth_it_names(#[case] expected: &[&str], #[case] pattern: &str) {
    let dir = project(&[
        ("dist/a.exe", b"a"),
        ("dist/b.txt", b"b"),
        ("dist/nested/c.exe", b"c"),
    ]);
    let plan = materialize_project(
        dir.path(),
        &format!("[[files]]\nsource = \"{pattern}\"\ndestination = \"${{install}}\"\n"),
    )
    .unwrap();
    let matched: Vec<_> = plan
        .files
        .iter()
        .map(|file| file.source_relative.as_str())
        .collect();
    assert_eq!(matched, expected, "{pattern}");
}

#[rstest]
#[case::by_default("bni/**/*.exe", false)]
#[case::when_the_author_says_so("bni/**/*.exe", true)]
fn a_pattern_that_matches_nothing_is_refused_unless_allowed(
    #[case] pattern: &str,
    #[case] allow_empty: bool,
) {
    let dir = project(&[("dist/a.txt", b"a")]);
    let files_block = format!(
        r#"
[[files]]
source = "{pattern}"
destination = "${{install}}"
allow_empty = {allow_empty}
"#
    );
    let outcome = materialize_project(dir.path(), &files_block);
    if allow_empty {
        assert!(outcome.unwrap().files.is_empty());
    } else {
        assert!(matches!(
            outcome.unwrap_err(),
            BuildError::PatternMatchedNothing { .. }
        ));
    }
}

#[test]
fn dotfiles_are_included() {
    let dir = project(&[("dist/.env", b"secret=1"), ("dist/visible.txt", b"v")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();
    let mut names: Vec<_> = plan
        .files
        .iter()
        .map(|f| f.source_relative.as_str().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".env".to_owned(), "visible.txt".to_owned()]);
}

#[test]
fn deterministic_across_insertion_orders() {
    let a = project(&[
        ("dist/z.txt", b"z"),
        ("dist/a.txt", b"a"),
        ("dist/m/n.txt", b"n"),
    ]);
    let b = project(&[
        ("dist/m/n.txt", b"n"),
        ("dist/a.txt", b"a"),
        ("dist/z.txt", b"z"),
    ]);

    let plan_a = materialize_project(
        a.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();
    let plan_b = materialize_project(
        b.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();

    let portable = |plan: &zup_build::TargetBuildPlan| {
        plan.files
            .iter()
            .map(|f| {
                (
                    f.source_relative.as_str().to_owned(),
                    f.destination.to_string(),
                    f.size,
                    f.sha256,
                    f.component.clone(),
                    f.condition.clone(),
                )
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(portable(&plan_a), portable(&plan_b));
    assert_eq!(plan_a.total_size, plan_b.total_size);
    assert_eq!(plan_a.files.len(), plan_b.files.len());
}

#[test]
fn large_file_streamed_with_correct_size() {
    let big = vec![0xABu8; 3 * 1024 * 1024];
    let dir = project(&[("dist/big.bin", &big)]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "big.bin"
destination = "${install}/big.bin"
"#,
    )
    .unwrap();
    assert_eq!(plan.files[0].size, big.len() as u64);
    assert_eq!(plan.total_size, big.len() as u64);

    let mut hasher = Sha256::new();
    hasher.update(&big);
    let expected = Sha256Digest::from_hasher(hasher);
    assert_eq!(plan.files[0].sha256, expected);
}

#[test]
fn different_sources_same_destination() {
    let dir = project(&[("dist/a/foo.dll", b"a"), ("dist/b/foo.dll", b"b")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "a/foo.dll"
destination = "${install}"

[[files]]
source = "b/foo.dll"
destination = "${install}"
"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, BuildError::DestinationCollision { .. }),
        "{err:?}"
    );
}

#[test]
fn overlapping_globs_same_destination() {
    let dir = project(&[("dist/foo.dll", b"a")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"

[[files]]
source = "*.dll"
destination = "${install}"
"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, BuildError::DestinationCollision { .. }),
        "{err:?}"
    );
}

#[rstest]
#[case::case_only_destinations(
    r#"
[[files]]
source = "x/Foo.dll"
destination = "${install}/Foo.dll"

[[files]]
source = "y/foo.dll"
destination = "${install}/foo.dll"
"#,
    2
)]
#[case::reserved_device_name(
    r#"
[[files]]
source = "file.txt"
destination = "${install}/CON"
"#,
    1
)]
#[case::trailing_dot(
    r#"
[[files]]
source = "file.txt"
destination = "${install}/bad."
"#,
    1
)]
fn windows_hostile_destinations_are_left_for_target_lowering(
    #[case] files_block: &str,
    #[case] expected_files: usize,
) {
    let dir = project(&[
        ("dist/x/Foo.dll", b"a"),
        ("dist/y/foo.dll", b"b"),
        ("dist/file.txt", b"x"),
    ]);
    let plan = materialize_project(dir.path(), files_block).unwrap();
    assert_eq!(plan.files.len(), expected_files);
}

#[cfg(unix)]
#[test]
fn symlink_file_rejected() {
    let dir = project(&[("dist/real.txt", b"real")]);
    std::os::unix::fs::symlink("real.txt", dir.path().join("dist/link.txt")).unwrap();

    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, BuildError::MatchedSymlink { .. }), "{err:?}");
}

#[cfg(unix)]
#[test]
fn symlink_directory_not_followed_or_matched_as_payload() {
    let dir = project(&[("dist/keep/a.txt", b"a"), ("outside/secret.txt", b"secret")]);
    std::os::unix::fs::symlink("../outside", dir.path().join("dist/secret")).unwrap();

    let result = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    );
    match result {
        Err(BuildError::MatchedSymlink { .. }) => {}
        Ok(plan) => {
            let names: Vec<_> = plan
                .files
                .iter()
                .map(|f| f.source_relative.as_str().to_owned())
                .collect();
            assert!(names.contains(&"keep/a.txt".to_owned()));
            assert!(!names.iter().any(|n| n.contains("secret")));
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[rstest]
#[case::a_climbing_pattern("../outside/*")]
#[case::an_unterminated_bracket("a/[")]
fn an_unusable_pattern_is_refused(#[case] pattern: &str) {
    assert!(matches!(
        FilePattern::compile(pattern),
        Err(BuildError::InvalidGlob { .. })
    ));
}

#[test]
fn preserves_component_condition_and_destination() {
    let dir = project(&[("dist/bin/tool.exe", b"tool")]);
    let source = manifest_toml(
        r#"
[[components]]
id = "cli"
name = "CLI"

[[files]]
source = "bin/**/*"
destination = "${install}/tools"
component = "cli"
when = 'component("cli")'
"#,
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let plan = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap();

    let file = &plan.files[0];
    assert_eq!(file.component.as_ref().unwrap().as_str(), "cli");
    assert!(file.condition.is_some());
    assert_eq!(file.destination.to_string(), "${install}/tools/tool.exe");
}

#[test]
fn source_paths_are_not_the_portable_identity() {
    let dir = project(&[("dist/a.txt", b"a")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "a.txt"
destination = "${install}/a.txt"
"#,
    )
    .unwrap();
    let file = &plan.files[0];
    assert!(file.source.is_absolute() || file.source.starts_with(dir.path()));
    assert_eq!(file.source_relative.as_str(), "a.txt");
    assert!(!file.source_relative.as_str().contains("C:\\"));
}

#[test]
fn resolves_plugin_sources_in_declaration_order() {
    let dir = project(&[
        ("dist/app.bin", b"app"),
        ("plugins/z.wasm", b"z"),
        ("plugins/a.wasm", b"a"),
    ]);
    let source = manifest_toml(
        r#"
[[plugins]]
id = "z-plugin"
source = "plugins/z.wasm"

[[plugins]]
id = "a-plugin"
source = "plugins/a.wasm"
"#,
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let plan = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap();
    assert_eq!(
        plan.plugins
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect::<Vec<_>>(),
        ["z-plugin", "a-plugin"]
    );
    assert_eq!(plan.plugins[0].source_relative.as_str(), "plugins/z.wasm");
    assert_eq!(plan.plugins[0].size, 1);
    assert_eq!(plan.plugins[1].source_relative.as_str(), "plugins/a.wasm");
}

#[rstest]
#[case::lexical_traversal("lexical")]
#[case::absolute_path("absolute")]
fn a_plugin_source_outside_the_project_is_refused(#[case] kind: &str) {
    let dir = project(&[("dist/app.bin", b"app"), ("outside.wasm", b"bad")]);
    let source = manifest_toml(
        r#"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
    );
    let mut manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    manifest.plugins[0].value.source = match kind {
        "lexical" => "../outside.wasm".to_owned(),
        _ => dir
            .path()
            .join("helper.wasm")
            .to_string_lossy()
            .into_owned(),
    };
    let error = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(matches!(
        error,
        BuildError::PluginSourceEscapesProject { .. } | BuildError::UnsafeRelativePath { .. }
    ));
}

#[test]
fn rejects_excess_plugin_declarations_before_source_access() {
    let dir = TempDir::new().unwrap();
    let plugins = (0..=MAX_PLUGIN_ARTIFACTS)
        .map(|index| {
            format!(
                "[[plugins]]\nid = \"plugin-{index}\"\nsource = \"plugins/missing-{index}.wasm\"\n"
            )
        })
        .collect::<String>();
    let source = manifest_toml(&plugins);
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let error = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(matches!(
        error,
        BuildError::TooManyPluginDeclarations {
            count,
            limit: MAX_PLUGIN_ARTIFACTS,
        } if count == MAX_PLUGIN_ARTIFACTS + 1
    ));
}

#[test]
fn rejects_oversized_plugin_source() {
    let dir = project(&[("dist/app.bin", b"app")]);
    let oversized = vec![0; zup_build::MAX_PLUGIN_SOURCE_BYTES as usize + 1];
    write_file(&dir.path().join("plugins/helper.wasm"), &oversized);
    let source = manifest_toml(
        r#"
[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let error = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(matches!(error, BuildError::PluginSourceTooLarge { .. }));
}

#[cfg(unix)]
#[test]
fn rejects_special_plugin_source() {
    let dir = project(&[("dist/app.bin", b"app")]);
    let socket = dir.path().join("plugins/helper.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let source = manifest_toml(
        r#"
[[plugins]]
id = "helper"
source = "plugins/helper.sock"
"#,
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let error = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(matches!(error, BuildError::PluginSourceNotRegular { .. }));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_plugin_source() {
    let dir = project(&[("dist/app.bin", b"app"), ("plugins/real.wasm", b"real")]);
    std::os::unix::fs::symlink("real.wasm", dir.path().join("plugins/link.wasm")).unwrap();
    let source = manifest_toml(
        r#"
[[plugins]]
id = "helper"
source = "plugins/link.wasm"
"#,
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source, "default").unwrap();
    let error = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(matches!(error, BuildError::PluginSourceSymlink { .. }));
}

const TARGET_MATRIX: &str = r#"
schema = 1

[app]
id = "com.example.matrix"
name = "Matrix"
version = "1.0.0"

[build]

[build.targets.linux-arm64]
target = "aarch64-unknown-linux-gnu"
source = { directory = "dist/linux-arm64" }

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Matrix"

[[files]]
source = "**/*"
destination = "${install}"
targets = ["linux-arm64", "windows-x64"]
"#;

fn selected_targets(
    source: &str,
) -> (
    zup_manifest::Manifest,
    Vec<(zup_core::ResolvedTargetConfig, zup_core::Installer)>,
) {
    let manifest = parse(source).expect("parse");
    let selected = select_targets(&manifest, &[], &TargetOverrides::default())
        .expect("targets")
        .into_iter()
        .map(|config| {
            let installer =
                compile(&manifest, &config, &TargetOverrides::default()).expect("compile");
            (config, installer)
        })
        .collect::<Vec<_>>();
    (manifest, selected)
}

fn materialize_targets(
    manifest_path: &Path,
    source: &str,
) -> Result<zup_build::BuildPlan, BuildError> {
    let (manifest, mut selected) = selected_targets(source);
    selected.reverse();
    materialize(manifest_path, &manifest, selected, Writes::None)
}

#[test]
fn target_filtered_plugin_materializes_only_for_its_profile() {
    let source = TARGET_MATRIX.replace(
        r#"targets = ["linux-arm64", "windows-x64"]"#,
        r#"targets = ["linux-arm64", "windows-x64"]

[[plugins]]
id = "linux-helper"
source = "plugins/linux-helper.wasm"
targets = ["linux-arm64"]"#,
    );
    let dir = project(&[
        ("dist/linux-arm64/app", b"linux"),
        ("dist/windows-x64/app", b"windows"),
        ("plugins/linux-helper.wasm", b"plugin"),
    ]);

    let plan = materialize_targets(&dir.path().join("zup.toml"), &source).unwrap();

    assert_eq!(plan.targets[0].plugins.len(), 1);
    assert_eq!(plan.targets[0].plugins[0].id.as_str(), "linux-helper");
    assert!(plan.targets[1].plugins.is_empty());
}

#[test]
fn target_matrix_materializes_independent_payloads_in_profile_order() {
    let dir = project(&[
        ("dist/linux-arm64/app", b"linux"),
        ("dist/windows-x64/app", b"windows"),
    ]);
    let plan = materialize_targets(&dir.path().join("zup.toml"), TARGET_MATRIX).unwrap();

    assert_eq!(plan.targets.len(), 2);
    assert_eq!(
        plan.targets
            .iter()
            .map(|target| target.installer.target.as_str())
            .collect::<Vec<_>>(),
        ["aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]
    );
    let linux = &plan.targets[0];
    let windows = &plan.targets[1];
    assert_eq!(linux.files[0].size, 5);
    assert_eq!(windows.files[0].size, 7);
    assert_ne!(linux.files[0].sha256, windows.files[0].sha256);
    assert_eq!(linux.total_size, 5);
    assert_eq!(windows.total_size, 7);
    assert!(linux.icons.fallback);
    assert!(windows.icons.fallback);
    assert!(linux.icons.artifacts.iter().all(|icon| matches!(
        icon.role,
        zup_core::IconRole::LinuxSvg | zup_core::IconRole::LinuxPng { .. }
    )));
    assert!(
        windows
            .icons
            .artifacts
            .iter()
            .all(|icon| icon.role == zup_core::IconRole::Windows)
    );
    assert_eq!(windows.icons.artifacts.len(), 1);
    assert!(!windows.icons.artifacts[0].executable_group.is_empty());
    assert_eq!(
        plan.target_by_triple(&TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()),
        Some(windows)
    );
}

#[test]
fn missing_source_is_attributed_to_its_target_profile() {
    let dir = project(&[("dist/windows-x64/app", b"windows")]);
    let error = materialize_targets(&dir.path().join("zup.toml"), TARGET_MATRIX).unwrap_err();

    assert!(error.to_string().contains("linux-arm64"), "{error}");
    assert!(matches!(error, BuildError::Target { .. }));
}

#[test]
fn destination_collisions_are_isolated_per_target() {
    let source = TARGET_MATRIX.replace(
        r#"source = "**/*"
destination = "${install}""#,
        r#"source = "a/**/*"
destination = "${install}/shared"
allow_empty = true

[[files]]
source = "b/**/*"
destination = "${install}/shared"
allow_empty = true"#,
    );
    let dir = project(&[
        ("dist/linux-arm64/a/foo", b"linux"),
        ("dist/windows-x64/b/foo", b"windows"),
    ]);
    let plan = materialize_targets(&dir.path().join("zup.toml"), &source).unwrap();

    assert_eq!(plan.targets[0].files.len(), 1);
    assert_eq!(plan.targets[1].files.len(), 1);
    assert_eq!(
        plan.targets[0].files[0].destination.to_string(),
        "${install}/shared/foo"
    );
    assert_eq!(
        plan.targets[1].files[0].destination.to_string(),
        "${install}/shared/foo"
    );
}

#[test]
fn target_matrix_materializes_shared_update_and_plugin_metadata() {
    let source = format!(
        r#"{}
[updates]
repository = "https://updates.example.test"
channel = "stable"
root = "keys/root.json"

[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
        TARGET_MATRIX
    );
    let dir = project(&[
        ("dist/linux-arm64/app", b"linux"),
        ("dist/windows-x64/app", b"windows"),
        ("keys/root.json", b"root"),
        ("plugins/helper.wasm", b"plugin"),
    ]);
    let plan = materialize_targets(&dir.path().join("zup.toml"), &source).unwrap();

    assert_eq!(
        plan.targets[0]
            .installer
            .updates
            .as_ref()
            .unwrap()
            .trusted_root,
        b"root"
    );
    assert_eq!(
        plan.targets[1]
            .installer
            .updates
            .as_ref()
            .unwrap()
            .trusted_root,
        b"root"
    );
    assert_eq!(plan.targets[0].plugins.len(), 1);
    assert_eq!(plan.targets[1].plugins.len(), 1);
    assert_eq!(plan.targets[0].plugins[0].size, 6);
    assert_eq!(plan.targets[1].plugins[0].size, 6);
    assert_eq!(
        plan.targets[0].plugins[0].sha256,
        plan.targets[1].plugins[0].sha256
    );
}

#[test]
fn selection_errors_are_rejected_before_filesystem_access() {
    let path = Path::new("missing-project/zup.toml");

    let (manifest, _) = selected_targets(TARGET_MATRIX);
    let error = materialize(path, &manifest, Vec::new(), Writes::None).unwrap_err();
    assert!(matches!(error, BuildError::EmptyTargetSelection));

    let (manifest, mut selected) = selected_targets(TARGET_MATRIX);
    selected[1].0.profile = selected[0].0.profile.clone();
    let error = materialize(path, &manifest, selected, Writes::None).unwrap_err();
    assert!(matches!(error, BuildError::DuplicateTargetProfile { .. }));

    let (manifest, mut selected) = selected_targets(TARGET_MATRIX);
    selected[1].0.target = selected[0].0.target.clone();
    selected[1].1.target = selected[1].0.target.clone();
    let error = materialize(path, &manifest, selected, Writes::None).unwrap_err();
    assert!(matches!(error, BuildError::DuplicateTarget { .. }));

    let (manifest, mut selected) = selected_targets(TARGET_MATRIX);
    selected[0].1.target = TargetTriple::parse("aarch64-pc-windows-msvc").unwrap();
    let error = materialize(path, &manifest, selected, Writes::None).unwrap_err();
    assert!(matches!(error, BuildError::TargetMismatch { .. }));
}

fn with_icon(source_line: &str) -> String {
    manifest_toml("").replace(
        "version = \"1.0.0\"\n",
        &format!("version = \"1.0.0\"\n{source_line}\n"),
    )
}

#[test]
fn a_project_icon_replaces_the_built_in_mark() {
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 8 8"><rect width="8" height="8" fill="#fff"/></svg>"##;
    let plain = project(&[("dist/app.exe", b"app")]);
    let branded = project(&[("dist/app.exe", b"app"), ("assets/icon.svg", svg)]);
    let plain_plan = materialize_project(plain.path(), "").unwrap();
    let source = with_icon("icon = \"assets/icon.svg\"");
    let manifest = parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source, "default").unwrap();
    let branded_plan =
        materialize_one(&branded.path().join("zup.toml"), &manifest, installer).unwrap();
    assert!(plain_plan.icons.fallback);
    assert!(!branded_plan.icons.fallback);
    assert_ne!(
        plain_plan.icons.artifacts[0].sha256,
        branded_plan.icons.artifacts[0].sha256
    );
}

#[test]
fn only_a_publishing_materialization_leaves_the_icon_cache() {
    let quiet = project(&[("dist/app.exe", b"app")]);
    let quiet_plan = materialize_project(quiet.path(), "").unwrap();
    assert!(
        !quiet.path().join(".zup").exists(),
        "a plan nobody asked to install must not leave anything behind"
    );

    let loud = project(&[("dist/app.exe", b"app")]);
    let (manifest, selected) = selected_targets(&manifest_toml(""));
    let loud_plan = materialize(
        &loud.path().join("zup.toml"),
        &manifest,
        selected,
        Writes::Publish,
    )
    .unwrap();

    assert_eq!(
        quiet_plan.icons.artifacts[0].sha256, loud_plan.targets[0].icons.artifacts[0].sha256,
        "the cache is a memo, so both must compile the same icon"
    );
    assert!(
        loud.path().join(".zup").join("icons").is_dir(),
        "a build publishes what it derived, so the next build can reuse it"
    );
    assert!(quiet_plan.icons.artifacts[0].source.is_none());
    assert!(loud_plan.targets[0].icons.artifacts[0].source.is_some());
}

#[test]
fn a_missing_icon_names_the_file() {
    let dir = project(&[("dist/app.exe", b"app")]);
    let source = with_icon("icon = \"assets/icon.svg\"");
    let manifest = parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source, "default").unwrap();
    let error = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(matches!(error, BuildError::IconMissing { .. }), "{error}");
    assert!(error.to_string().contains("icon.svg"), "{error}");
}

#[test]
fn a_small_raster_warns_and_still_builds() {
    let png: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0xf3, 0xff, 0x61, 0x00, 0x00, 0x00, 0x19, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x9f, 0x12, 0xcc, 0x30, 0x6a, 0xc0, 0xa8, 0x01, 0xa3, 0x06, 0x0c, 0x17,
        0x03, 0x00, 0x30, 0xc4, 0xfe, 0x10, 0x1c, 0x27, 0xe4, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let dir = project(&[("dist/app.exe", b"app"), ("assets/icon.png", png)]);
    let source = with_icon("icon = \"assets/icon.png\"");
    let manifest = parse(&source).unwrap();
    let installer = zup_manifest::parse_and_compile(&source, "default").unwrap();
    let plan = materialize_one(&dir.path().join("zup.toml"), &manifest, installer).unwrap();
    assert!(
        plan.icons.warnings[0].contains("16×16"),
        "{}",
        plan.icons.warnings[0]
    );
    assert_eq!(plan.icons.artifacts.len(), 1);
}
