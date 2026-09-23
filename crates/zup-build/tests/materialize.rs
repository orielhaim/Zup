//! Integration tests for source materialization.

use std::fs;
use std::path::Path;

use rstest::rstest;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zup_build::{BuildError, FilePattern, Sha256Digest, materialize};
use zup_manifest::parse_and_compile;

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

[source]
directory = "dist"

[install]
scope = "user"

[install.directory]
user = "${{known.local_app_data}}/Acme"

{files_block}
"#
    )
}

fn materialize_project(dir: &Path, files_block: &str) -> Result<zup_build::BuildPlan, BuildError> {
    let source = manifest_toml(files_block);
    let manifest = zup_manifest::parse(&source).expect("parse");
    let installer = zup_manifest::parse_and_compile(&source).expect("compile");
    let manifest_path = dir.join("zup.toml");
    materialize(&manifest_path, &manifest, installer)
}

// --- Source root ---

#[test]
fn source_root_is_relative_to_manifest_not_cwd() {
    let dir = project(&[("dist/acme.exe", b"bin")]);
    // Nested project root; CWD is unrelated.
    let nested = dir.path().join("project");
    fs::create_dir_all(nested.join("dist")).unwrap();
    write_file(&nested.join("dist/acme.exe"), b"bin");
    fs::write(nested.join("zup.toml"), b"").unwrap();

    let source = manifest_toml(
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    );
    let manifest = zup_manifest::parse(&source).unwrap();
    let installer = parse_and_compile(&source).unwrap();
    let plan = materialize(&nested.join("zup.toml"), &manifest, installer).unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].source_relative.as_str(), "acme.exe");
}

#[test]
fn missing_source_root() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("zup.toml"), b"").unwrap();
    let err = materialize_project(dir.path(), "").unwrap_err();
    assert!(matches!(err, BuildError::SourceMissing { .. }), "{err:?}");
}

#[test]
fn source_not_directory() {
    let dir = project(&[("dist", b"file-not-dir")]);
    // `dist` is a file
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
    let installer = parse_and_compile(&source).unwrap();
    let err = materialize(&dir.path().join("zup.toml"), &manifest, installer).unwrap_err();
    assert!(
        matches!(err, BuildError::SourceEscapesProject { .. }),
        "{err:?}"
    );
}

// --- Globs and static-root mapping ---

#[test]
fn glob_star_star_preserves_tree() {
    let dir = project(&[("dist/acme.exe", b"exe"), ("dist/helpers/foo.dll", b"dll")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();

    let dests: Vec<_> = plan
        .files
        .iter()
        .map(|f| f.destination.to_string())
        .collect();
    assert_eq!(
        dests,
        [
            "${install}/acme.exe".to_owned(),
            "${install}/helpers/foo.dll".to_owned(),
        ]
    );
}

#[test]
fn glob_bin_strips_bin_prefix() {
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

#[test]
fn glob_assets_icons_strips_prefix() {
    let dir = project(&[
        ("dist/assets/icons/app.png", b"png"),
        ("dist/assets/other.png", b"png"),
    ]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "assets/icons/*.png"
destination = "${install}/icons"
"#,
    )
    .unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(
        plan.files[0].destination.to_string(),
        "${install}/icons/app.png"
    );
}

#[test]
fn glob_exe_extension() {
    let dir = project(&[
        ("dist/a.exe", b"a"),
        ("dist/b.txt", b"b"),
        ("dist/nested/c.exe", b"c"),
    ]);
    // `*.exe` is not recursive
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "*.exe"
destination = "${install}"
"#,
    )
    .unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].source_relative.as_str(), "a.exe");

    // `**/*.exe` is recursive
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*.exe"
destination = "${install}"
"#,
    )
    .unwrap();
    assert_eq!(plan.files.len(), 2);
}

#[test]
fn nested_matching() {
    let dir = project(&[
        ("dist/x/1.txt", b"1"),
        ("dist/x/y/2.txt", b"2"),
        ("dist/x/y/z/3.txt", b"3"),
    ]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "x/**/*"
destination = "${install}"
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
            "${install}/1.txt".to_owned(),
            "${install}/y/2.txt".to_owned(),
            "${install}/y/z/3.txt".to_owned(),
        ]
    );
}

#[test]
fn invalid_glob_rejected() {
    let dir = project(&[("dist/a.txt", b"a")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "a/["
destination = "${install}"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, BuildError::InvalidGlob { .. }), "{err:?}");
}

#[test]
fn empty_match_rejected_by_default() {
    let dir = project(&[("dist/a.txt", b"a")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "bni/**/*.exe"
destination = "${install}"
"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, BuildError::PatternMatchedNothing { .. }),
        "{err:?}"
    );
}

#[test]
fn allow_empty_permits_zero_matches() {
    let dir = project(&[("dist/a.txt", b"a")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "bni/**/*.exe"
destination = "${install}"
allow_empty = true
"#,
    )
    .unwrap();
    assert!(plan.files.is_empty());
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
fn windows_separators_normalized_in_pattern() {
    let dir = project(&[("dist/bin/acme.exe", b"exe")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "bin\\**\\*"
destination = "${install}"
"#,
    )
    .unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].destination.to_string(), "${install}/acme.exe");
}

// --- Determinism ---

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

    let portable = |plan: &zup_build::BuildPlan| {
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

// --- Hashing ---

#[test]
fn known_sha256_empty_file() {
    let dir = project(&[("dist/empty.txt", b"")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "empty.txt"
destination = "${install}/empty.txt"
"#,
    )
    .unwrap();
    assert_eq!(plan.files[0].size, 0);
    assert_eq!(
        plan.files[0].sha256.to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn known_sha256_fixture() {
    // SHA-256("abc")
    let dir = project(&[("dist/abc.txt", b"abc")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "abc.txt"
destination = "${install}/abc.txt"
"#,
    )
    .unwrap();
    assert_eq!(plan.files[0].size, 3);
    assert_eq!(
        plan.files[0].sha256.to_hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
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
fn duplicate_content_allowed() {
    let dir = project(&[
        ("dist/a/config.dat", b"same"),
        ("dist/b/config.dat", b"same"),
    ]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "a/**/*"
destination = "${install}/a"

[[files]]
source = "b/**/*"
destination = "${install}/b"
"#,
    )
    .unwrap();
    assert_eq!(plan.files.len(), 2);
    assert_eq!(plan.files[0].sha256, plan.files[1].sha256);
}

// --- Collisions ---

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

#[test]
fn case_only_windows_collision() {
    let dir = project(&[("dist/x/Foo.dll", b"a"), ("dist/y/foo.dll", b"b")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "x/Foo.dll"
destination = "${install}/Foo.dll"

[[files]]
source = "y/foo.dll"
destination = "${install}/foo.dll"
"#,
    )
    .unwrap_err();
    assert!(
        matches!(err, BuildError::WindowsDestinationCollision { .. }),
        "{err:?}"
    );
}

#[test]
fn same_filename_different_directories_ok() {
    let dir = project(&[("dist/a/foo.dll", b"a"), ("dist/b/foo.dll", b"b")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "a/foo.dll"
destination = "${install}/a/foo.dll"

[[files]]
source = "b/foo.dll"
destination = "${install}/b/foo.dll"
"#,
    )
    .unwrap();
    assert_eq!(plan.files.len(), 2);
}

// --- Path safety ---

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

    // `**/*` should not walk into the symlink dir; if the link itself matches,
    // it must be rejected rather than packaged.
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

#[test]
fn literal_dot_dot_pattern_rejected() {
    let pattern = FilePattern::compile("../outside/*");
    assert!(matches!(pattern, Err(BuildError::InvalidGlob { .. })));
}

#[test]
fn invalid_windows_names_rejected() {
    let dir = project(&[("dist/CON", b"x")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "CON"
destination = "${install}/CON"
"#,
    )
    .unwrap_err();
    // Either the source filename maps to CON (invalid) or destination CON is invalid.
    assert!(
        matches!(
            err,
            BuildError::InvalidWindowsDestinationName { .. }
                | BuildError::UnsafeRelativePath { .. }
                | BuildError::PathNotRepresentable { .. }
        ),
        "{err:?}"
    );
}

#[test]
fn trailing_dot_destination_rejected() {
    let dir = project(&[("dist/file.txt", b"x")]);
    let err = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "file.txt"
destination = "${install}/bad."
"#,
    )
    .unwrap_err();
    // Destination is fully specified including filename via static-root parent mapping:
    // source `file.txt` → suffix `file.txt` → `${install}/bad./file.txt` — `bad.` is invalid.
    // OR if destination is the full path... let's also try exact.
    match err {
        BuildError::InvalidWindowsDestinationName { .. } => {}
        other => {
            // If suffix appended, `bad.` is still a literal segment.
            panic!("expected invalid Windows name, got {other:?}");
        }
    }
}

// --- Metadata preservation ---

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
    let installer = parse_and_compile(&source).unwrap();
    let plan = materialize(&dir.path().join("zup.toml"), &manifest, installer).unwrap();

    let file = &plan.files[0];
    assert_eq!(file.component.as_ref().unwrap().as_str(), "cli");
    assert!(file.condition.is_some());
    assert_eq!(file.destination.to_string(), "${install}/tools/tool.exe");
    // Destination variables remain unresolved.
    assert!(file.destination.to_string().contains("${install}"));
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
    // Portable path must not contain the temp-dir absolute prefix as its serialized form.
    assert!(!file.source_relative.as_str().contains("C:\\"));
}

// --- Static root unit coverage ---

#[rstest]
#[case("bin/**/*", "bin/acme.exe", "acme.exe")]
#[case("bin/**/*", "bin/helpers/foo.dll", "helpers/foo.dll")]
#[case("**/*", "a/b/c.txt", "a/b/c.txt")]
#[case("assets/icons/*.png", "assets/icons/app.png", "app.png")]
#[case("*.exe", "acme.exe", "acme.exe")]
fn static_root_mapping(#[case] pattern: &str, #[case] matched: &str, #[case] suffix: &str) {
    let compiled = FilePattern::compile(pattern).unwrap();
    assert_eq!(compiled.destination_suffix(matched).unwrap(), suffix);
}

#[test]
fn total_size_sums_all_files() {
    let dir = project(&[("dist/a.txt", b"aaa"), ("dist/b.txt", b"bb")]);
    let plan = materialize_project(
        dir.path(),
        r#"
[[files]]
source = "**/*"
destination = "${install}"
"#,
    )
    .unwrap();
    assert_eq!(plan.total_size, 5);
    assert_eq!(plan.files.len(), 2);
}
