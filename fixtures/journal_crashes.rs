use std::path::Path;
use zup_transaction::{
    FilesystemTransactionStore, StoreError, TransactionRecord, TransactionStore,
};

pub fn child(open: impl Fn(&Path) -> FilesystemTransactionStore) {
    let Some(root) = std::env::var_os("ZUP_JOURNAL_CRASH_ROOT") else {
        return;
    };
    let record: TransactionRecord = serde_json::from_slice(
        &std::fs::read(std::env::var_os("ZUP_JOURNAL_CRASH_RECORD").unwrap()).unwrap(),
    )
    .unwrap();
    let store = open(Path::new(&root));
    let result = if std::env::var_os("ZUP_JOURNAL_CAS").is_some() {
        let mut next = record;
        next.touch();
        store.compare_and_swap(0, &next)
    } else {
        store.create(&record)
    };
    match result {
        Ok(()) => {}
        Err(StoreError::AlreadyExists { .. }) => std::process::exit(42),
        Err(StoreError::RevisionConflict { .. }) => std::process::exit(43),
        Err(error) => panic!("journal child failed: {error:?}"),
    }
}

fn command(root: &Path, input: &Path, test: &str, ignored: bool) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test])
        .env("ZUP_JOURNAL_CRASH_ROOT", root)
        .env("ZUP_JOURNAL_CRASH_RECORD", input);
    if ignored {
        command.arg("--ignored");
    }
    command
}

pub fn matrix(
    open: impl Fn(&Path) -> FilesystemTransactionStore,
    original: TransactionRecord,
    test: &str,
    ignored: bool,
) {
    for point in [
        "journal_namespace_created",
        "journal_lock_published",
        "journal_creation_reserved",
        "journal_initial_log_published",
        "journal_initial_head_published",
        "journal_record_published",
        "journal_namespace_published",
        "journal_parent_sync_before",
        "journal_parent_synced",
        "journal_reservation_remove_before",
        "journal_reservation_removed",
        "journal_creation_retired",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = open(&root);
        store.create(&original).unwrap();
        let mut proposed = original.clone();
        proposed.transaction_id = zup_core::TransactionId::new_v7();
        let input = temp.path().join("input.json");
        std::fs::write(&input, serde_json::to_vec(&proposed).unwrap()).unwrap();
        let status = command(&root, &input, test, ignored)
            .env("ZUP_TEST_FAULT_POINT", point)
            .status()
            .unwrap();
        assert!(!status.success(), "child must reach {point}");
        let store = open(&root);
        for id in store.list_ids().unwrap() {
            store.load(&id).unwrap();
        }
        assert_eq!(store.load(&original.transaction_id).unwrap(), original);
        let mut unrelated = original.clone();
        unrelated.transaction_id = zup_core::TransactionId::new_v7();
        if matches!(
            point,
            "journal_namespace_created" | "journal_lock_published"
        ) {
            assert_eq!(store.list_ids().unwrap(), [original.transaction_id]);
            assert!(matches!(
                store.create(&unrelated),
                Err(StoreError::CreationDeferred { .. })
            ));
            assert!(root.join("journal-creation/pending").exists());
        } else {
            if store.list_ids().unwrap().contains(&proposed.transaction_id) {
                let mut advanced = proposed.clone();
                advanced.touch();
                store.compare_and_swap(0, &advanced).unwrap();
                store.create(&unrelated).unwrap();
                assert_eq!(store.load(&proposed.transaction_id).unwrap(), advanced);
            } else {
                store.create(&unrelated).unwrap();
                assert_eq!(store.load(&proposed.transaction_id).unwrap(), proposed);
            }
            assert!(!root.join("journal-creation/pending").exists());
            assert!(!root.join("journal-creation/pending.json").exists());
            assert!(matches!(
                store.create(&proposed),
                Err(StoreError::AlreadyExists { .. })
            ));
        }
    }
    for point in ["journal_cas_before", "journal_cas_published"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = open(&root);
        store.create(&original).unwrap();
        let input = temp.path().join("input.json");
        std::fs::write(&input, serde_json::to_vec(&original).unwrap()).unwrap();
        let lock_path = root
            .join("transactions")
            .join(original.transaction_id.to_string())
            .join("transaction.lock");
        let identity =
            zup_storage::namespace::identity(&std::fs::File::open(&lock_path).unwrap()).unwrap();
        assert!(
            !command(&root, &input, test, ignored)
                .env("ZUP_JOURNAL_CAS", "1")
                .env("ZUP_TEST_FAULT_POINT", point)
                .status()
                .unwrap()
                .success()
        );
        let store = open(&root);
        let revision = if point == "journal_cas_before" { 0 } else { 1 };
        assert_eq!(
            store.load(&original.transaction_id).unwrap().revision,
            revision
        );
        let mut next = store.load(&original.transaction_id).unwrap();
        next.touch();
        store.compare_and_swap(revision, &next).unwrap();
        assert_eq!(
            zup_storage::namespace::identity(&std::fs::File::open(lock_path).unwrap()).unwrap(),
            identity
        );
    }
}

fn wait_ready(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not reach barrier"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub fn races(
    open: impl Fn(&Path) -> FilesystemTransactionStore,
    original: TransactionRecord,
    test: &str,
    ignored: bool,
) {
    for cas in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = open(&root);
        let input = temp.path().join("input.json");
        std::fs::write(&input, serde_json::to_vec(&original).unwrap()).unwrap();
        if cas {
            store.create(&original).unwrap();
        }
        let latch = temp.path().join("barrier");
        let mut first = command(&root, &input, test, ignored);
        first
            .env(
                "ZUP_TEST_FAULT_POINT",
                if cas {
                    "journal_cas_before"
                } else {
                    "journal_creation_reserved"
                },
            )
            .env("ZUP_TEST_FAULT_ACTION", "pause")
            .env("ZUP_TEST_FAULT_LATCH", &latch);
        if cas {
            first.env("ZUP_JOURNAL_CAS", "1");
        }
        let mut first = first.spawn().unwrap();
        wait_ready(&latch.with_extension("ready"));
        if !cas {
            assert!(store.list_ids().unwrap().is_empty());
        }
        let mut second = command(&root, &input, test, ignored);
        if cas {
            second.env("ZUP_JOURNAL_CAS", "1");
        }
        let mut second = second.spawn().unwrap();
        std::fs::write(latch.with_extension("release"), b"go").unwrap();
        assert!(first.wait().unwrap().success());
        assert_eq!(
            second.wait().unwrap().code(),
            Some(if cas { 43 } else { 42 })
        );
        assert_eq!(store.list_ids().unwrap(), [original.transaction_id]);
        assert_eq!(
            store.load(&original.transaction_id).unwrap().revision,
            u64::from(cas)
        );
    }
}
