#![allow(dead_code)]
mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
use zup_plan::RemovalKind;
use zup_plan::{
    BackendOperation, DeploymentEffects, DeploymentPlan, FileRemoval, compile_deployment,
};
pub mod fake;

use zup_core::TargetPath;
use zup_core::{
    BackendResourceId, Privilege, RelativePath, ResourceKey, SelectedScope, Sha256Digest, Target,
    hash_reader,
};
use zup_plan::{FileOperation, FileOperationKind, FilePrecondition};

pub fn target() -> Target {
    Target::parse("windows-x64").unwrap()
}

pub fn owned_instance() -> zup_core::FixedFileInstance {
    zup_core::FixedFileInstance {
        origin: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
        object: zup_core::NativeObjectIdentity::Windows {
            volume: 1,
            file: [1; 16],
        },
    }
}

pub fn installation_id() -> zup_core::InstallationId {
    "00000000-0000-7000-8000-000000000001".parse().unwrap()
}

#[derive(Default)]
pub struct Publisher(pub std::sync::Mutex<Option<zup_plan::InstallationRecord>>);

impl zup_transaction::InstallationPublisher for Publisher {
    type Error = String;

    fn prepare(
        &self,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_plan::InstallationRecord, String> {
        zup_transaction::prepare_installation_record(transaction, self.0.lock().unwrap().as_ref())
            .map_err(|error| error.to_string())
    }

    fn publish(
        &self,
        transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_plan::InstallationRecord, String> {
        let mut durable = self.0.lock().unwrap();
        let installation =
            zup_transaction::prepare_installation_record(transaction, durable.as_ref())
                .map_err(|error| error.to_string())?;
        *durable = Some(installation.clone());
        Ok(installation)
    }
}

pub fn effects() -> DeploymentEffects {
    let mut effects = DeploymentEffects::new(
        installation_id(),
        requirements_fixture::requirements(
            target(),
            zup_core::SelectedScope::User,
            sample_app_id().as_str(),
            &sample_version().to_string(),
        ),
    );
    effects.install_directory = Some(tpath(r"C:\PF\Acme"));
    effects
}

pub fn installed(scope: SelectedScope) -> zup_plan::InstallationRecord {
    {
        let fixture_commit = zup_plan::InstallationCommit {
            transaction: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
            plan_hash: digest(b"installed fixture"),
        };
        zup_plan::InstallationRecord {
            commit: fixture_commit,
            retired_deployments: Vec::new(),
            data: Default::default(),
            id: installation_id(),
            state: zup_plan::InstallationState::Installed,
            app_id: sample_app_id(),
            scope,
            target: target(),
            directory: tpath(r"C:\PF\Acme"),
            current: zup_plan::DeploymentSnapshot {
                maintenance: None,
                requirements: requirements_fixture::requirements(
                    target(),
                    scope,
                    sample_app_id().as_str(),
                    &sample_version().to_string(),
                ),
                revision: zup_core::ActivationRevision::FIRST,
                transaction: fixture_commit.transaction,
                plan_hash: fixture_commit.plan_hash,
                version: sample_version(),
                selected_components: Vec::new(),
                release: None,
                preset: None,
                materialization: None,
                resources: Default::default(),
            },
            retained: Vec::new(),
        }
    }
}

pub fn reserve(
    store: &impl zup_transaction::TransactionStore,
    record: &mut zup_transaction::TransactionRecord,
) {
    use zup_transaction::{NodeState, OperationReceipt};
    let id = zup_plan::OperationId::new(zup_plan::OperationId::RESERVE);
    *record = store
        .update(&record.transaction_id, &mut |fresh| {
            fresh.nodes.insert(id.clone(), NodeState::Running);
            Ok(())
        })
        .unwrap();
    let reservation = store.reserve_installation(record).unwrap();
    *record = store
        .update(&record.transaction_id, &mut |fresh| {
            fresh.reservation = Some(reservation.clone());
            fresh.nodes.insert(
                id.clone(),
                NodeState::Applied {
                    receipt: Box::new(OperationReceipt::Control),
                },
            );
            Ok(())
        })
        .unwrap();
}

pub fn tpath(value: &str) -> TargetPath {
    TargetPath::new(target(), value).unwrap()
}

pub fn digest(bytes: &[u8]) -> Sha256Digest {
    hash_reader(bytes).unwrap().1
}

/// A file work item whose executable intent is explicit.
///
/// The flag is a parameter rather than hard-coded so a test can ask for the
/// truth it wants to assert: an executor that reported `true` for every file
/// would pass every test that only ever builds a non-executable payload.
pub fn file_with(name: &str, contents: &[u8], executable: bool) -> FileOperation {
    FileOperation {
        executable,
        ..file(name, contents)
    }
}

pub fn file(name: &str, contents: &[u8]) -> FileOperation {
    FileOperation {
        replacement: zup_core::FixedReplacement::Atomic,
        key: ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::TreeMember {
                    declaration: zup_core::ResourceId::new("fixture").unwrap(),
                    member: zup_core::RelativePath::new(name).unwrap(),
                },
            },
        },
        source_relative: RelativePath::new(name).unwrap(),
        destination: tpath(&format!(r"C:\PF\Acme\{name}")),
        precondition: FilePrecondition::Absent,
        expected_sha256: digest(contents),
        expected_size: contents.len() as u64,
        privilege: Privilege::User,
        kind: FileOperationKind::Create,
        executable: false,

        conflict: None,
    }
}

fn backend() -> BackendOperation {
    let id = BackendResourceId::new("fake.backend").unwrap();
    BackendOperation::apply(
        ResourceKey::Backend { id: id.clone() },
        id,
        Privilege::User,
        b"opaque-backend-payload".to_vec(),
    )
}

pub fn sample_input() -> DeploymentEffects {
    let mut input = effects();
    input.files = vec![file("a.exe", b"a"), file("b.dll", b"b")];
    input.backend_operations = vec![backend()];
    input
}

pub fn chain_input() -> DeploymentEffects {
    let mut input = sample_input();
    input.files.push(file("c.txt", b"c"));
    input.backend_operations.clear();
    input
}

pub fn removal_input() -> DeploymentEffects {
    let mut input = effects();
    let key = ResourceKey::File {
        id: zup_core::FileResourceId::Payload {
            id: zup_core::PayloadId::File {
                declaration: zup_core::ResourceId::new("fixture-1").unwrap(),
            },
        },
    };
    input.retired_keys.push(key.clone());
    input.removals.push(FileRemoval {
        instance: owned_instance(),
        key,
        kind: RemovalKind::RemoveOwned,
        scope: SelectedScope::User,
        privilege: Privilege::User,
        destination: tpath(r"C:\PF\Acme\old.exe"),
        sha256: digest(b"old"),
        size: 3,
        created_directories: Vec::new(),
    });
    input
}

pub fn sample_plan() -> DeploymentPlan {
    compile_deployment(&sample_input()).unwrap()
}

pub fn sample_app_id() -> zup_core::AppId {
    zup_core::AppId::new("com.acme.acme").unwrap()
}

pub fn sample_version() -> semver::Version {
    "1.4.0".parse().unwrap()
}
