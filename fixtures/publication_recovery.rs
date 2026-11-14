use zup_transaction::{
    InstallationPublisher, PublicationObservation, TransactionRecord, TransactionStore,
};

struct Interrupted<P> {
    native: P,
    before_write: bool,
}

impl<P: InstallationPublisher> InstallationPublisher for Interrupted<P> {
    type Error = String;
    fn read_current(
        &self,
        record: &TransactionRecord,
    ) -> Result<Option<InstallationRecord>, String> {
        self.native.read_current(record).map_err(|e| e.to_string())
    }
    fn prepare(&self, record: &TransactionRecord) -> Result<InstallationRecord, String> {
        self.native.prepare(record).map_err(|e| e.to_string())
    }
    fn publish(
        &self,
        record: &TransactionRecord,
        observed: &PublicationObservation,
    ) -> Result<InstallationRecord, String> {
        if !self.before_write {
            self.native
                .publish(record, observed)
                .map_err(|e| e.to_string())?;
        }
        Err("publication interrupted".into())
    }
}

#[rstest::rstest]
#[case::source_free_intact(false, false)]
#[case::replaced_native_object(false, true)]
#[case::committed_then_drifted(true, true)]
fn native_publication_recovery_preserves_commit_and_requires_current_object_authority(
    #[case] already_published: bool,
    #[case] replace_fixed: bool,
) {
    #[cfg(windows)]
    let root = tempfile::tempdir_in(std::env::var_os("LOCALAPPDATA").unwrap()).unwrap();
    #[cfg(not(windows))]
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let install = root.path().join("install");
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"generation").unwrap();
    std::fs::write(source.join("fixed-source"), b"fixed").unwrap();
    let mut effects = deployment_effects(&install, &source, b"generation", None, None);
    let fixed = install.join("fixed.txt");
    effects.files.push(zup_plan::FileOperation {
        replacement: zup_core::FixedReplacement::Atomic,
        key: zup_core::ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::File {
                    declaration: zup_core::ResourceId::new("fixed").unwrap(),
                },
            },
        },
        destination: TargetPath::new(
            *effects.generations[0].materialization.directory().target(),
            fixed.to_string_lossy(),
        )
        .unwrap(),
        source_relative: RelativePath::new("fixed-source").unwrap(),
        precondition: zup_plan::FilePrecondition::Absent,
        expected_size: 5,
        expected_sha256: hash_bytes(b"fixed"),
        executable: false,
        kind: zup_plan::FileOperationKind::Create,
        privilege: Privilege::User,
        conflict: None,
    });
    let store = FilesystemTransactionStore::private_at(&state);
    let open = TransactionCoordinator::new(store.clone())
        .begin(
            AppId::new("example.generations").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            compile_deployment(&effects).unwrap(),
        )
        .unwrap();
    let _lock = zup_transaction::ResourceLock::try_acquire(
        &state,
        &zup_transaction::ResourceLock::lock_key("example.generations", "user"),
    )
    .unwrap()
    .unwrap();
    let publisher = Interrupted {
        native: native_publisher(&state),
        before_write: !already_published,
    };
    let mut executor = native_executor(&state, &source, open.record());
    let interrupted = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(
        interrupted.outcome,
        TransactionOutcome::RecoveryRequired,
        "{:?}",
        interrupted.record.failure
    );
    drop(executor);
    std::fs::rename(&source, root.path().join("unavailable-source")).unwrap();
    if replace_fixed {
        std::fs::rename(&fixed, install.join("external-instance")).unwrap();
        std::fs::write(&fixed, b"fixed").unwrap();
    }
    let mut executor = native_executor(&state, &source, &interrupted.record);
    let (record, outcome) = zup_transaction::recover(
        store.load(&interrupted.record.transaction_id).unwrap(),
        &store,
        &mut executor,
        &publisher.native,
    )
    .unwrap();
    let expected = if replace_fixed && !already_published {
        TransactionOutcome::RecoveryRequired
    } else {
        TransactionOutcome::Committed
    };
    assert_eq!(outcome, expected, "{:?}", record.recovery_failure);
    assert_eq!(std::fs::read(&fixed).unwrap(), b"fixed");
    assert_eq!(
        publisher.native.read_current(&record).unwrap().is_some(),
        outcome == TransactionOutcome::Committed
    );
    assert!(!source.exists());
    assert!(install.exists());
}
