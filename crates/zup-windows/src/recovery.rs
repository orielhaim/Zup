use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use private_fs::{Durability, PrivateDir, Publication};
use serde::{Deserialize, Serialize};
use zup_bundle::{PayloadError, PayloadReader, PayloadSource};
use zup_core::{RelativePath, SelectedScope, Sha256Digest, TransactionId};
use zup_plan::{GenerationOperation, NodeKind};
use zup_transaction::{NodeState, ReconciliationState, TransactionPhase, TransactionRecord};

use crate::filesystem::{MachineDir, PolicyKind};
use crate::machine_authority::MachineRoot;

// Bump whenever an execution semantic changes, even if the JSON still parses.
const EXECUTION_FORMAT: u32 = 2;
const DIRECTORY: &str = "recovery";
const CONTEXT: &str = "source.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Blob {
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    execution_format: u32,
    transaction: TransactionId,
    plan: Sha256Digest,
    release: Option<zup_core::ReleaseIdentity>,
    overlay: Sha256Digest,
    files: BTreeMap<RelativePath, Blob>,
}

impl Context {
    fn for_record(record: &TransactionRecord) -> Result<Self, String> {
        record.validate().map_err(display)?;
        let files = required_files(&record.plan)?;
        Ok(Self {
            execution_format: EXECUTION_FORMAT,
            transaction: record.transaction_id,
            plan: record.plan_hash,
            release: record.release.clone(),
            overlay: crate::PayloadOverlayIdentity::from_transaction(
                record.app_id.clone(),
                record.app_version.clone(),
                record.scope,
                &record.plan,
            )
            .map_err(display)?
            .digest(),
            files,
        })
    }
}

pub(crate) fn required_files(
    plan: &zup_plan::DeploymentPlan,
) -> Result<BTreeMap<RelativePath, Blob>, String> {
    let mut files = BTreeMap::new();
    let mut insert = |path: RelativePath, blob: Blob| -> Result<(), String> {
        if files
            .insert(path, blob.clone())
            .is_some_and(|prior| prior != blob)
        {
            return Err("transaction assigns different content to one source".into());
        }
        Ok(())
    };
    for node in &plan.nodes {
        match &node.kind {
            NodeKind::StageFile { .. } => insert(
                node.meta
                    .source_relative
                    .clone()
                    .ok_or("stage has no source")?,
                Blob {
                    size: node.meta.expected_size.ok_or("stage has no size")?,
                    sha256: node.meta.expected_sha256.ok_or("stage has no digest")?,
                },
            )?,
            NodeKind::Generation {
                operation: GenerationOperation::Materialize,
                intent,
            } => {
                for member in intent.materialization.descriptor().files() {
                    if let Some(source) = intent.sources.get(&member.path) {
                        insert(
                            source.source_relative.clone(),
                            Blob {
                                size: member.size,
                                sha256: member.sha256,
                            },
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(files)
}

enum Directory {
    User(PrivateDir),
    Machine(MachineDir),
}

impl Directory {
    fn transaction(root: &Path, record: &TransactionRecord) -> Result<Self, String> {
        match record.scope {
            SelectedScope::User => PrivateDir::open(root)
                .and_then(|dir| dir.open_dir("transactions"))
                .and_then(|dir| dir.open_dir(record.transaction_id.to_string()))
                .map(Self::User)
                .map_err(display),
            SelectedScope::Machine => MachineRoot::from_existing(root, &record.app_id)
                .and_then(|machine| machine.transaction_directory(record.transaction_id))
                .map(Self::Machine)
                .map_err(display),
        }
    }

    fn source(&self, create: bool) -> Result<Self, String> {
        match self {
            Self::User(dir) => if create {
                dir.ensure_dir(DIRECTORY)
            } else {
                dir.open_dir(DIRECTORY)
            }
            .map(Self::User)
            .map_err(display),
            Self::Machine(dir) => if create {
                dir.ensure_dir(DIRECTORY, PolicyKind::Administrators)
            } else {
                dir.open_dir(DIRECTORY, PolicyKind::Administrators)
            }
            .map(Self::Machine)
            .map_err(display),
        }
    }

    fn open(&self, name: &str) -> Result<Box<dyn ReadSeek>, String> {
        match self {
            Self::User(dir) => dir
                .open_file(name)
                .map(|f| Box::new(f) as Box<dyn ReadSeek>)
                .map_err(display),
            Self::Machine(dir) => dir
                .open_generation_file(name, zup_transaction::GenerationAccess::Private)
                .map(|f| Box::new(f) as Box<dyn ReadSeek>)
                .map_err(display),
        }
    }

    fn publish(&self, name: &str, reader: &mut dyn Read, blob: &Blob) -> Result<(), String> {
        match self {
            Self::User(dir) => {
                let mut output = dir
                    .atomic_with_policy(name, Durability::Full, Publication::CreateNew)
                    .map_err(display)?;
                let mut hash = sha2::Sha256::default();
                use sha2::Digest;
                let mut size = 0;
                let mut buffer = [0; 64 * 1024];
                let mut bounded = reader.take(blob.size.saturating_add(1));
                loop {
                    let n = bounded.read(&mut buffer).map_err(display)?;
                    if n == 0 {
                        break;
                    }
                    output.write_all(&buffer[..n]).map_err(display)?;
                    hash.update(&buffer[..n]);
                    size += n as u64;
                }
                if size != blob.size || Sha256Digest::from_hasher(hash) != blob.sha256 {
                    return Err("source bytes differ from the authorized transaction".into());
                }
                output.commit().map_err(display)?;
            }
            Self::Machine(dir) => dir
                .stage_generation_file(name, reader, blob.size, blob.sha256)
                .map_err(display)?,
        }
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, String> {
        match self {
            Self::User(dir) => dir
                .entries()
                .map_err(display)?
                .into_iter()
                .map(|name| {
                    name.into_string()
                        .map_err(|_| "non-Unicode recovery member".into())
                })
                .collect(),
            Self::Machine(dir) => dir.list().map_err(display),
        }
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        match self {
            Self::User(dir) => dir.remove_file(name, Durability::Full).map_err(display),
            Self::Machine(dir) => dir.remove(name).map_err(display),
        }
    }

    fn identity(&self) -> Result<(u64, [u8; 16]), String> {
        match self {
            Self::User(dir) => crate::filesystem::identity(dir).map_err(display),
            Self::Machine(dir) => dir.object_identity().map_err(display),
        }
    }

    fn remove_source(&self, expected: (u64, [u8; 16])) -> Result<(), String> {
        match self {
            Self::User(dir) => {
                crate::filesystem::remove_empty_directory(dir, DIRECTORY, expected)
                    .map_err(display)?;
                dir.sync_all().map_err(display)
            }
            Self::Machine(dir) => dir
                .remove_empty_directory(DIRECTORY, expected)
                .map_err(display),
        }
    }
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

pub(crate) struct RetainedSource {
    directory: Option<Directory>,
    files: BTreeMap<RelativePath, Blob>,
}

impl RetainedSource {
    pub fn empty() -> Self {
        Self {
            directory: None,
            files: BTreeMap::new(),
        }
    }

    pub fn capture(
        root: &Path,
        record: &TransactionRecord,
        payload: &dyn PayloadSource,
    ) -> Result<Self, String> {
        let context = Context::for_record(record)?;
        let directory = Directory::transaction(root, record)?.source(true)?;
        let executor = crate::WindowsFileExecutor::new(
            Self::empty(),
            record.transaction_id,
            record.target.executable_suffix(),
        )
        .with_state_authority(root.to_path_buf(), record.scope, &record.app_id);
        let mut present: BTreeSet<_> = directory.names()?.into_iter().collect();
        for (path, blob) in &context.files {
            let name = blob.sha256.to_hex();
            if present.contains(&name) {
                verify_blob(&directory, &name, blob)?;
                continue;
            }
            let retained = record
                .plan
                .nodes
                .iter()
                .find(|node| {
                    matches!(node.kind, NodeKind::StageFile { .. })
                        && node.meta.source_relative.as_ref() == Some(path)
                })
                .map(|node| executor.retained_reader(node, record, blob.size, blob.sha256))
                .transpose()
                .map_err(display)?
                .flatten();
            let mut input = match retained {
                Some(reader) => reader,
                None => payload
                    .open(path, &blob.sha256, blob.size)
                    .map_err(display)?,
            };
            directory.publish(&name, &mut input, blob)?;
            present.insert(name.clone());
            verify_blob(&directory, &name, blob)?;
        }
        let bytes = serde_json::to_vec(&context).map_err(display)?;
        let (size, sha256) = zup_core::hash_reader(bytes.as_slice()).map_err(display)?;
        directory.publish(CONTEXT, &mut bytes.as_slice(), &Blob { size, sha256 })?;
        Self::load_required(root, record)
    }

    pub fn load(root: &Path, record: &TransactionRecord) -> Result<Self, String> {
        // No effects have escaped an untouched preparation. It can be retired without content.
        if settled(record)
            || record
                .nodes
                .values()
                .all(|state| matches!(state, NodeState::Pending))
        {
            let parent = Directory::transaction(root, record)?;
            if parent.names()?.iter().any(|name| name == DIRECTORY) {
                let directory = parent.source(false)?;
                if directory.names()?.iter().any(|name| name == CONTEXT) {
                    read_context(&directory, &Context::for_record(record)?)?;
                }
            }
            return Ok(Self::empty());
        }
        Self::load_required(root, record).map_err(|error| format!("Required retained transaction source is unavailable or incompatible: {error}. The transaction remains pending. Restore its exact protected source from backup or use a trusted carrier supporting its execution format."))
    }

    fn load_required(root: &Path, record: &TransactionRecord) -> Result<Self, String> {
        let expected = Context::for_record(record)?;
        let directory = Directory::transaction(root, record)?.source(false)?;
        let context = read_context(&directory, &expected)?;
        for blob in context.files.values() {
            verify_blob(&directory, &blob.sha256.to_hex(), blob)?;
        }
        Ok(Self {
            directory: Some(directory),
            files: context.files,
        })
    }
}

fn read_context(directory: &Directory, expected: &Context) -> Result<Context, String> {
    let mut bytes = Vec::new();
    directory
        .open(CONTEXT)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(display)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("recovery context exceeds its limit".into());
    }
    let context: Context = serde_json::from_slice(&bytes).map_err(display)?;
    if context.execution_format != EXECUTION_FORMAT {
        return Err("this maintenance carrier cannot execute the retained transaction format; use a compatible trusted carrier".into());
    }
    if &context != expected {
        return Err("retained source differs from the protected transaction".into());
    }
    Ok(context)
}

fn verify_blob(
    directory: &Directory,
    name: &str,
    blob: &Blob,
) -> Result<Box<dyn ReadSeek>, String> {
    let mut file = directory.open(name)?;
    if zup_core::hash_reader((&mut file).take(blob.size.saturating_add(1))).map_err(display)?
        != (blob.size, blob.sha256)
    {
        return Err("retained recovery content is missing or changed; restore the exact protected source before retrying".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(display)?;
    Ok(file)
}

impl PayloadSource for RetainedSource {
    fn open(
        &self,
        path: &RelativePath,
        sha256: &Sha256Digest,
        size: u64,
    ) -> Result<PayloadReader, PayloadError> {
        let error = |detail: String| PayloadError::Read {
            path: path.to_string(),
            source: std::io::Error::other(detail),
        };
        let blob = self
            .files
            .get(path)
            .filter(|blob| blob.sha256 == *sha256 && blob.size == size)
            .ok_or_else(|| {
                error("payload is not declared by the protected recovery source".into())
            })?;
        let file = verify_blob(
            self.directory
                .as_ref()
                .ok_or_else(|| error("recovery source unavailable".into()))?,
            &sha256.to_hex(),
            blob,
        )
        .map_err(error)?;
        Ok(file)
    }
}

pub(crate) fn settled(record: &TransactionRecord) -> bool {
    record.phase == TransactionPhase::RolledBack
        || record.phase == TransactionPhase::Committed
            && record.cleanup_complete()
            && record.reconciliation == ReconciliationState::Complete
}

pub(crate) fn collect(root: &Path, record: &TransactionRecord) -> Result<(), String> {
    if !settled(record) {
        return Ok(());
    }
    let parent = Directory::transaction(root, record)?;
    if !parent.names()?.iter().any(|name| name == DIRECTORY) {
        return Ok(());
    }
    let directory = parent.source(false)?;
    let expected = Context::for_record(record)?;
    let names = directory.names()?;
    let declared: BTreeSet<_> = expected
        .files
        .values()
        .map(|blob| blob.sha256.to_hex())
        .chain(std::iter::once(CONTEXT.to_owned()))
        .collect();
    if names.iter().any(|name| !declared.contains(name)) {
        return Err("unknown entry in retained recovery source; collection deferred".into());
    }
    if names.iter().any(|name| name == CONTEXT) {
        read_context(&directory, &expected)?;
    }
    for name in names.iter().filter(|name| name.as_str() != CONTEXT) {
        directory.remove(name)?;
    }
    if names.iter().any(|name| name == CONTEXT) {
        directory.remove(CONTEXT)?;
    }
    let object = directory.identity()?;
    drop(directory);
    parent.remove_source(object)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{AppId, Privilege, ResourceKey, Target, TargetPath};
    use zup_plan::{
        DeploymentEffects, FileOperation, FileOperationKind, FilePrecondition, compile_deployment,
    };
    use zup_transaction::{FilesystemTransactionStore, TransactionCoordinator};

    mod requirements_fixture {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/deployment.rs"
        ));
    }

    fn fixture(root: &Path, scope: SelectedScope) -> (std::path::PathBuf, TransactionRecord) {
        let target = Target::host().unwrap();
        let app = AppId::new(format!(
            "example.recovery-{}",
            uuid::Uuid::now_v7().simple()
        ))
        .unwrap();
        let mut effects = DeploymentEffects::new(
            zup_core::InstallationId::new_v7(),
            requirements_fixture::requirements(target, scope, app.as_str(), "2.0.0"),
        );
        let directory =
            TargetPath::new(target, root.join("application").to_string_lossy()).unwrap();
        effects.install_directory = Some(directory.clone());
        effects.files.push(FileOperation {
            replacement: zup_core::FixedReplacement::Atomic,
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::File {
                        declaration: zup_core::ResourceId::new("generated").unwrap(),
                    },
                },
            },
            source_relative: RelativePath::new("__zup_plugins__/generated.bin").unwrap(),
            destination: TargetPath::new(
                target,
                root.join("application/generated.bin").to_string_lossy(),
            )
            .unwrap(),
            precondition: FilePrecondition::Absent,
            expected_sha256: zup_core::hash_bytes(b"authenticated"),
            expected_size: 13,
            privilege: if scope == SelectedScope::Machine {
                Privilege::System
            } else {
                Privilege::User
            },
            kind: FileOperationKind::Create,
            executable: false,
            conflict: None,
        });
        let (state, store) = match scope {
            SelectedScope::User => (
                root.join("state"),
                FilesystemTransactionStore::private_at(root.join("state")),
            ),
            SelectedScope::Machine => {
                let machine = MachineRoot::acquire(&app).unwrap();
                (machine.path().to_path_buf(), machine.transaction_store())
            }
        };
        let record = TransactionCoordinator::new(store)
            .begin(
                app,
                scope,
                "2.0.0".parse().unwrap(),
                compile_deployment(&effects).unwrap(),
            )
            .unwrap()
            .preserve();
        (state, record)
    }

    fn capture(root: &Path, state: &Path, record: &TransactionRecord) -> RetainedSource {
        let original = root.join("original");
        std::fs::create_dir_all(original.join("__zup_plugins__")).unwrap();
        std::fs::write(
            original.join("__zup_plugins__/generated.bin"),
            b"authenticated",
        )
        .unwrap();
        let source = zup_bundle::DirectoryPayloadSource::new(&original);
        let retained = RetainedSource::capture(state, record, &source).unwrap();
        std::fs::remove_dir_all(&original).unwrap();
        retained
    }

    fn check_bytes(source: &RetainedSource) {
        let mut bytes = Vec::new();
        source
            .open(
                &RelativePath::new("__zup_plugins__/generated.bin").unwrap(),
                &zup_core::hash_bytes(b"authenticated"),
                13,
            )
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"authenticated");
    }

    #[test]
    fn retained_generated_source_is_exact_private_and_kept_until_settlement() {
        let root =
            tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
        let (state, mut record) = fixture(root.path(), SelectedScope::User);
        let source = capture(root.path(), &state, &record);
        check_bytes(&source);
        drop(source);
        collect(&state, &record).unwrap();
        check_bytes(&RetainedSource::load_required(&state, &record).unwrap());
        record.phase = TransactionPhase::RolledBack;
        for state in record.nodes.values_mut() {
            *state = NodeState::RolledBack;
        }
        collect(&state, &record).unwrap();
        assert!(
            !state
                .join("transactions")
                .join(record.transaction_id.to_string())
                .join(DIRECTORY)
                .exists()
        );
    }

    #[test]
    fn compacted_journal_reopens_retained_source_after_original_overlay_disappears() {
        use zup_transaction::TransactionStore;
        let root =
            tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
        let (state, mut record) = fixture(root.path(), SelectedScope::User);
        drop(capture(root.path(), &state, &record));
        let store = FilesystemTransactionStore::private_at(&state);
        for i in 0..48 {
            record = store
                .transition(
                    &record.transaction_id,
                    record.revision,
                    &[zup_transaction::TransactionChange::RecoveryFailure(Some(
                        zup_transaction::TransactionFailure {
                            operation: None,
                            message: format!("attempt {i}: {}", "retained evidence ".repeat(500)),
                        },
                    ))],
                )
                .unwrap();
        }
        let journal = state
            .join("transactions")
            .join(record.transaction_id.to_string());
        assert!(journal.join("checkpoint-b.json").exists());
        assert!(!root.path().join("original").exists());
        let cold = FilesystemTransactionStore::private_at(&state);
        let durable = cold.load(&record.transaction_id).unwrap();
        assert_eq!(durable, record);
        check_bytes(&RetainedSource::load_required(&state, &durable).unwrap());
        collect(&state, &durable).unwrap();
        check_bytes(&RetainedSource::load_required(&state, &durable).unwrap());
    }

    #[rstest::rstest]
    #[case("missing")]
    #[case("changed")]
    #[case("format")]
    #[case("plan")]
    #[case("overlay")]
    #[case("release")]
    fn unavailable_or_substituted_source_is_refused_without_fallback(#[case] change: &str) {
        let root =
            tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
        let (state, record) = fixture(root.path(), SelectedScope::User);
        drop(capture(root.path(), &state, &record));
        let directory = state
            .join("transactions")
            .join(record.transaction_id.to_string())
            .join(DIRECTORY);
        let blob = directory.join(zup_core::hash_bytes(b"authenticated").to_hex());
        match change {
            "missing" => std::fs::remove_file(blob).unwrap(),
            "changed" => std::fs::write(blob, b"substituted!!").unwrap(),
            _ => {
                let path = directory.join(CONTEXT);
                let mut context: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                match change {
                    "format" => context["execution_format"] = (EXECUTION_FORMAT + 1).into(),
                    "plan" => {
                        context["plan"] =
                            serde_json::to_value(zup_core::hash_bytes(b"foreign plan")).unwrap()
                    }
                    "overlay" => {
                        context["overlay"] =
                            serde_json::to_value(zup_core::hash_bytes(b"foreign overlay")).unwrap()
                    }
                    "release" => context["release"] = serde_json::json!({"untrusted": true}),
                    _ => unreachable!(),
                }
                zup_storage::publish_private(&path, &serde_json::to_vec(&context).unwrap())
                    .unwrap();
            }
        }
        assert!(RetainedSource::load_required(&state, &record).is_err());
        if change == "format" {
            let mut settled = record.clone();
            settled.phase = TransactionPhase::RolledBack;
            for state in settled.nodes.values_mut() {
                *state = NodeState::RolledBack;
            }
            assert!(RetainedSource::load(&state, &record).is_err());
            assert!(RetainedSource::load(&state, &settled).is_err());
            assert!(collect(&state, &settled).is_err());
            assert!(
                directory
                    .join(zup_core::hash_bytes(b"authenticated").to_hex())
                    .exists()
            );
        }

        assert!(directory.exists());
    }

    #[test]
    #[ignore = "requires an elevated Windows token"]
    fn machine_runtime_recovery_source_is_private_and_independent_of_original_overlay() {
        assert!(crate::is_process_elevated().unwrap());
        let root =
            tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
        let (state, mut record) = fixture(root.path(), SelectedScope::Machine);
        drop(capture(root.path(), &state, &record));
        check_bytes(&RetainedSource::load_required(&state, &record).unwrap());
        collect(&state, &record).unwrap();
        check_bytes(&RetainedSource::load_required(&state, &record).unwrap());
        record.phase = TransactionPhase::RolledBack;
        for state in record.nodes.values_mut() {
            *state = NodeState::RolledBack;
        }
        collect(&state, &record).unwrap();
    }
}
