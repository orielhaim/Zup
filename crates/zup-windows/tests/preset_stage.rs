#![cfg(windows)]

#[test]
fn staged_presets_are_private_verified_and_isolated() {
    let first =
        zup_windows::stage_session("preset.exe", &[("preset.exe", b"preset-one")]).expect("stage");
    let second =
        zup_windows::stage_session("preset.exe", &[("preset.exe", b"preset-two")]).expect("stage");
    assert_ne!(first.root, second.root);
    assert_eq!(
        std::fs::read(&first.executable).expect("read"),
        b"preset-one"
    );
    std::fs::write(&first.executable, b"replaced").expect("tamper");
    assert!(zup_preset_host::confirm_bytes(&first.executable, b"preset-one").is_err());
    let root = first.root.clone();
    drop(zup_preset_host::PresetStage::acquire(first.root));
    assert!(!root.exists(), "the session removes its staging directory");
    assert!(
        second.root.exists(),
        "another session keeps its own directory"
    );
    drop(zup_preset_host::PresetStage::acquire(second.root));
}

#[test]
fn an_existing_file_is_not_trusted_as_the_preset() {
    let parent = tempfile::tempdir().expect("parent");
    let occupied = parent.path().join("preset.exe");
    std::fs::write(&occupied, b"not the preset").expect("plant");
    assert!(occupied.is_file());
    let staged =
        zup_windows::stage_session("preset.exe", &[("preset.exe", b"real-preset")]).expect("stage");
    assert_ne!(staged.executable, occupied);
    assert_eq!(
        std::fs::read(&occupied).expect("planted"),
        b"not the preset"
    );
    assert_eq!(
        std::fs::read(&staged.executable).expect("staged"),
        b"real-preset"
    );
}
