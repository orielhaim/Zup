use rstest::rstest;
use zup_core::{SelectedScope, Target};
use zup_preview::Scenario;

#[rstest]
#[case(Target::WINDOWS_X64, SelectedScope::User)]
#[case(Target::LINUX_X64, SelectedScope::User)]
#[case(Target::LINUX_X64, SelectedScope::Machine)]
fn application_preview_plans_actual_files_for_the_selected_target(
    #[case] target: Target,
    #[case] scope: SelectedScope,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("dist")).unwrap();
    std::fs::write(root.path().join("dist/payload.bin"), b"payload").unwrap();
    let text = format!(
        r#"
schema = 1
[app]
id = "com.example.preview"
name = "Preview"
version = "1.0.0"
[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}
[install]
scope = "either"
[install.directory]
user = "${{location.user_data}}/Preview"
machine = "${{location.programs}}/Preview"
[[files]]
source = "**/*"
destination = "${{install}}"
"#
    );
    let manifest = zup_manifest::parse(&text).unwrap();
    let config = zup_manifest::select_targets(&manifest, &["default"], &Default::default())
        .unwrap()
        .remove(0);
    let installer = zup_manifest::compile(&manifest, &config, &Default::default()).unwrap();
    let build = zup_build::materialize(
        &root.path().join("zup.toml"),
        &manifest,
        vec![(config, installer)],
        zup_build::Writes::None,
    )
    .unwrap();
    let scenario = Scenario::from_build(build).unwrap();
    let selection = zup_preset_host::Selection {
        scope,
        components: Vec::new(),
        install_directory: None,
    };
    let preview = scenario.plan(&selection).unwrap();
    assert_eq!(preview.estimated_bytes, 7);
    let changes = preview
        .groups
        .iter()
        .filter(|group| group.category == zup_presentation::ResourceCategory::Files)
        .flat_map(|group| &group.changes)
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 1);
    assert!(
        changes[0]
            .location
            .as_ref()
            .unwrap()
            .ends_with("payload.bin")
    );
    assert!(!preview.install_directory.contains("${"));
    if target == Target::LINUX_X64 {
        assert!(preview.install_directory.starts_with('/'));
    }
}
