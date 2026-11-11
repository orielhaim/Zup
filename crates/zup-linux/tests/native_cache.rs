#![cfg(target_os = "linux")]

mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use std::path::Path;

use zup_core::{
    AppId, InstallationId, Privilege, RelativePath, ResourceKey, SelectedScope, Target, TargetPath,
    hash_bytes,
};
use zup_linux::{LinuxFileExecutor, LinuxInstallationStore};
use zup_plan::{
    DeploymentEffects, DeploymentNode, FileOperation, FileOperationKind, FilePrecondition,
    NativeCacheKind, NativeCacheOperation, NodeKind, compile_deployment,
};
use zup_transaction::{
    FilesystemTransactionStore, NodeState, OperationExecutor, OperationReceipt, ReconcileResult,
    RollbackReconcileResult, TransactionCoordinator, TransactionOutcome, TransactionRecord,
    TransactionStore,
};

const PACKAGE: &[u8] = b"<?xml version=\"1.0\"?><mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\"><mime-type type=\"application/x-zup-native-test\"><glob pattern=\"*.zup-native-test\"/></mime-type></mime-info>";

fn plan(root: &Path) -> zup_plan::DeploymentPlan {
    let target = Target::LINUX_X64;
    let directory = TargetPath::new(target, root.join("mime").to_string_lossy()).unwrap();
    let destination = directory.join("packages/test.xml").unwrap();
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
            },
        },
    };
    let mut effects = DeploymentEffects::new(
        InstallationId::new_v7(),
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.cache-test",
            "1.0.0",
        ),
    );
    effects.install_directory =
        Some(TargetPath::new(target, root.join("application").to_string_lossy()).unwrap());
    effects.files.push(FileOperation {
        key: key.clone(),
        destination,
        source_relative: RelativePath::new("package.xml").unwrap(),
        precondition: FilePrecondition::Absent,
        expected_sha256: hash_bytes(PACKAGE),
        expected_size: PACKAGE.len() as u64,
        privilege: Privilege::User,
        kind: FileOperationKind::Create,
        executable: false,
        conflict: None,
        replacement: zup_core::FixedReplacement::Atomic,
    });
    effects.native_caches.push(NativeCacheOperation {
        kind: NativeCacheKind::Mime,
        directory,
        sources: [key].into(),
    });
    compile_deployment(&effects).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    NativeOutput,
    LostCompletion,
    BeforeCache,
}

struct LoseCompletion {
    inner: LinuxFileExecutor,
    failure: Failure,
    native_output_seen: bool,
}

impl OperationExecutor for LoseCompletion {
    type Error = String;
    fn prepare(&mut self, node: &DeploymentNode) -> Result<(), String> {
        self.inner.prepare(node).map_err(|error| error.to_string())
    }
    fn apply(
        &mut self,
        node: &DeploymentNode,
        record: &TransactionRecord,
    ) -> Result<OperationReceipt, String> {
        let result = self.inner.apply(node, record);
        if let NodeKind::NativeCache { operation } = &node.kind {
            self.native_output_seen =
                std::fs::read_to_string(Path::new(operation.directory.as_str()).join("globs2"))
                    .is_ok_and(|contents| contents.contains("application/x-zup-native-test"));
        }
        let receipt = result.map_err(|error| error.to_string())?;
        if (self.failure == Failure::LostCompletion
            && matches!(node.kind, NodeKind::NativeCache { .. }))
            || (self.failure == Failure::BeforeCache
                && matches!(node.kind, NodeKind::FileMutation { .. }))
        {
            return Err("simulated loss of completion evidence".into());
        }
        Ok(receipt)
    }
    fn verify(&mut self, node: &DeploymentNode, receipt: &OperationReceipt) -> Result<(), String> {
        self.inner
            .verify(node, receipt)
            .map_err(|error| error.to_string())
    }
    fn rollback(
        &mut self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<(), String> {
        if self.failure != Failure::NativeOutput
            && matches!(node.kind, NodeKind::NativeCache { .. })
        {
            return Err("simulated interruption before cache compensation".into());
        }
        self.inner
            .rollback(node, receipt)
            .map_err(|error| error.to_string())
    }
    fn reconcile(
        &mut self,
        node: &DeploymentNode,
        receipt: Option<&OperationReceipt>,
        record: &TransactionRecord,
    ) -> Result<ReconcileResult, String> {
        self.inner
            .reconcile(node, receipt, record)
            .map_err(|error| error.to_string())
    }
    fn reconcile_rollback(
        &mut self,
        node: &DeploymentNode,
        receipt: &OperationReceipt,
    ) -> Result<RollbackReconcileResult, String> {
        OperationExecutor::reconcile_rollback(&mut self.inner, node, receipt)
            .map_err(|error| error.to_string())
    }
}

#[rstest::rstest]
#[case::native_partial_output_failure(Failure::NativeOutput)]
#[case::simulated_missing_receipt_and_interrupted_compensation(Failure::LostCompletion)]
#[case::simulated_failure_before_forward_cache_invocation(Failure::BeforeCache)]
fn recovery_regenerates_from_restored_sources(#[case] failure: Failure) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir_all(root.join("payload")).unwrap();
    std::fs::write(root.join("payload/package.xml"), PACKAGE).unwrap();
    if failure == Failure::NativeOutput {
        std::fs::create_dir_all(root.join("mime/mime.cache")).unwrap();
    }
    let plan = plan(root);
    let store = FilesystemTransactionStore::private_at(root.join("state"));
    let publisher = LinuxInstallationStore::new(root.join("state"));
    let open = TransactionCoordinator::new(store.clone())
        .begin(
            AppId::new("example.cache-test").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan.clone(),
        )
        .unwrap();
    let transaction = open.record().transaction_id;
    let mut inner = LinuxFileExecutor::new().with_payload(zup_bundle::DirectoryPayloadSource::new(
        root.join("payload"),
    ));
    inner.register_plan(&plan).unwrap();
    inner.set_state_authority(
        root.join("state"),
        open.record().app_id.clone(),
        open.record().scope,
    );
    inner.set_transaction_id(transaction);
    let mut executor = LoseCompletion {
        inner,
        failure,
        native_output_seen: false,
    };
    let settled = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(
        settled.outcome,
        TransactionOutcome::RecoveryRequired,
        "{:?}",
        settled.record.failure
    );
    assert!(!root.join("mime/packages/test.xml").exists());
    assert_eq!(executor.native_output_seen, failure != Failure::BeforeCache);
    let cache = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::NativeCache { .. }))
        .unwrap();
    assert!(matches!(
        settled.record.nodes[&cache.id],
        NodeState::RollingBack { .. }
    ));
    drop(executor);
    if failure == Failure::NativeOutput {
        std::fs::remove_dir(root.join("mime/mime.cache")).unwrap();
    }
    let mut recovered = LinuxFileExecutor::new();
    recovered.register_plan(&plan).unwrap();
    recovered.set_state_authority(
        root.join("state"),
        settled.record.app_id.clone(),
        settled.record.scope,
    );
    recovered.set_transaction_id(transaction);
    if failure == Failure::BeforeCache {
        let link = root.join("mime/foreign-link");
        std::os::unix::fs::symlink(root.join("payload/package.xml"), &link).unwrap();
        let (blocked, outcome) = zup_transaction::recover(
            store.load(&transaction).unwrap(),
            &store,
            &mut recovered,
            &publisher,
        )
        .unwrap();
        assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
        assert!(blocked.recovery_failure.is_some());
        assert!(matches!(
            blocked.nodes[&cache.id],
            NodeState::RollingBack { .. }
        ));
        std::fs::remove_file(link).unwrap();
    }
    let (_, outcome) = zup_transaction::recover(
        store.load(&transaction).unwrap(),
        &store,
        &mut recovered,
        &publisher,
    )
    .unwrap();
    assert_eq!(outcome, TransactionOutcome::RolledBack);
    assert!(
        !std::fs::read_to_string(root.join("mime/globs2"))
            .unwrap()
            .contains("application/x-zup-native-test")
    );
    assert!(
        publisher
            .load(
                &AppId::new("example.cache-test").unwrap(),
                SelectedScope::User
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn independent_processes_share_the_directory_lease() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temporary.path().join("mime/packages")).unwrap();
    let plan = plan(temporary.path());
    let cache = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::NativeCache { .. }))
        .unwrap();
    let mut executor = LinuxFileExecutor::new();
    executor.prepare(cache).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_child", "--ignored"])
        .env(
            "ZUP_CACHE_LEASE_TEST_DIRECTORY",
            temporary.path().join("mime"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(executor);
    let directory = std::fs::File::open(temporary.path().join("mime")).unwrap();
    directory.try_lock().unwrap();
}

#[rstest::rstest]
#[case::symbolic_link(false)]
#[case::hard_link(true)]
fn cache_generators_cannot_follow_links_to_unrelated_files(#[case] hard_link: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir_all(root.join("mime/packages")).unwrap();
    std::fs::write(root.join("unrelated"), b"preserve").unwrap();
    if hard_link {
        std::fs::hard_link(root.join("unrelated"), root.join("mime/globs2")).unwrap();
    } else {
        std::os::unix::fs::symlink(root.join("unrelated"), root.join("mime/globs2")).unwrap();
    }
    let plan = plan(root);
    let cache = plan
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::NativeCache { .. }))
        .unwrap();
    assert!(LinuxFileExecutor::new().prepare(cache).is_err());
    assert_eq!(std::fs::read(root.join("unrelated")).unwrap(), b"preserve");
}

#[test]
#[ignore = "invoked by the isolated lease test"]
fn lease_child() {
    let directory =
        std::fs::File::open(std::env::var_os("ZUP_CACHE_LEASE_TEST_DIRECTORY").unwrap()).unwrap();
    assert!(matches!(
        directory.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
}
