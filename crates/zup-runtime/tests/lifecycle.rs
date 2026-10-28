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
    assert!(discover_recovery(&root.0.join("state")).unwrap().is_empty());
}

#[test]
fn recovery_discovery_rejects_unreadable_transaction_state() {
    let root = TestDir::new();
    std::fs::write(root.0.join("transactions"), b"not a directory").unwrap();
    assert!(discover_recovery(&root.0).is_err());
}

#[test]
fn recovery_discovery_never_hides_a_missing_or_corrupt_journal() {
    let root = TestDir::new();
    let journal = root
        .0
        .join("transactions")
        .join(zup_runtime::Uuid::now_v7().to_string());
    std::fs::create_dir_all(&journal).unwrap();
    assert!(discover_recovery(&root.0).is_err());
    std::fs::write(journal.join("transaction.json"), b"{truncated").unwrap();
    assert!(discover_recovery(&root.0).is_err());
    assert!(discover_recovery(&root.0).is_err());
    assert_eq!(
        std::fs::read(journal.join("transaction.json")).unwrap(),
        b"{truncated"
    );
}

#[test]
fn recovery_discovery_rejects_unrecognized_transaction_directories() {
    let root = TestDir::new();
    std::fs::create_dir_all(root.0.join("transactions/invalid-identity")).unwrap();
    assert!(discover_recovery(&root.0).is_err());
}
