use std::path::Path;

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

#[test]
fn runtime_sources_do_not_mention_platform_adapter_dependencies() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in walk(&source_root) {
        let source = std::fs::read_to_string(&entry)
            .unwrap()
            .to_ascii_lowercase();
        let adapter = b"zup-windows"
            .iter()
            .map(|byte| *byte as char)
            .collect::<String>();
        let forbidden_package = b"windows-registry"
            .iter()
            .map(|byte| *byte as char)
            .collect::<String>();
        assert!(!source.contains(&adapter));
        assert!(!source.contains(&forbidden_package));
    }
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}
