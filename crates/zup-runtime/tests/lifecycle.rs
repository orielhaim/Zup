use zup_runtime::discover_recovery;

struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("zup-recovery-{}", zup_runtime::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn recovery_discovery_is_empty_for_a_new_state_root() {
    let root = TestDir::new();
    assert!(discover_recovery(&root.0.join("state")).is_empty());
}
