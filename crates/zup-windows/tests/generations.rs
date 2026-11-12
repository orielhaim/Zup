#![cfg(windows)]

use zup_transaction::TransactionStore;

mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}
mod interrupted_payload {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/interrupted_payload.rs"
    ));
}

mod publication_recovery {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/publication_recovery.rs"
    ));
}
mod persistent_lifecycle {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/persistent_lifecycle.rs"
    ));
}
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use zup_core::{
    ActivationRevision, AppId, GenerationDescriptor, GenerationFile, InstallationId,
    MaterializationId, MaterializationSpec, Privilege, RelativePath, SelectedScope, Target,
    TargetPath, hash_bytes,
};
use zup_plan::{
    DeploymentEffects, GenerationIntent, GenerationPreparation, InstallationRecord,
    compile_deployment,
};
use zup_transaction::{FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome};
use zup_windows::{InstallationRecordStore, WindowsFileExecutor};

fn deploy(
    state: &Path,
    root: &Path,
    source: &Path,
    contents: &[u8],
    prior: Option<&InstallationRecord>,
) -> InstallationRecord {
    commit_effects(
        state,
        source,
        deployment_effects(root, source, contents, prior, None),
    )
}

fn deployment_effects(
    root: &Path,
    source: &Path,
    contents: &[u8],
    prior: Option<&InstallationRecord>,
    claims: Option<&zup_plan::PersistentClaimSelection>,
) -> DeploymentEffects {
    let target = Target::WINDOWS_X64;
    let installation = prior
        .map(|record| record.id)
        .or_else(|| claims.map(|claims| claims.installation))
        .unwrap_or_else(InstallationId::new_v7);
    let id = MaterializationId::new_v7();
    let directory = TargetPath::new(target, root.to_string_lossy()).unwrap();
    let path = RelativePath::new("lib/data.txt").unwrap();
    let descriptor = GenerationDescriptor::new(
        target,
        vec![GenerationFile {
            path: path.clone(),
            size: contents.len() as u64,
            sha256: hash_bytes(contents),
            executable: false,
        }],
    )
    .unwrap();
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.generations",
            "1.0.0",
        ),
    );
    effects.observed = prior.map(|record| record.current.selection());
    effects.observed_transaction = prior.map(|record| record.current.transaction);
    effects.install_directory = Some(directory.clone());
    effects.requirements.directory = zup_core::Template::parse(directory.as_str()).unwrap();
    let data = zup_core::PersistentDirectory {
        id: zup_core::ResourceId::new("state").unwrap(),
        location: zup_core::PersistentLocation::Path {
            path: TargetPath::new(target, root.with_extension("data").to_string_lossy()).unwrap(),
            access: zup_core::PersistentAccess::Owner,
        },
        compatibility: Default::default(),
    };
    effects.requirements.data.push(
        data.map_path(|path| zup_core::Template::parse(path.as_str()))
            .unwrap(),
    );
    let intent = zup_plan::PersistentDirectoryIntent {
        installation,
        previous: prior
            .and_then(|record| record.data.get(&data.id))
            .or_else(|| claims.and_then(|claims| claims.owners.get(&data.id)))
            .cloned(),
        binding: zup_plan::PersistentBinding::Filesystem(zup_plan::FilesystemDataBinding {
            path: data.location.path().unwrap().clone(),
            writers: Vec::new(),
        }),
        directory: data,
        privilege: Privilege::User,
    };
    for stage in [
        zup_plan::PersistentDirectoryStage::Prepare,
        zup_plan::PersistentDirectoryStage::Access,
    ] {
        effects.data.push(zup_plan::PersistentDirectoryOperation {
            intent: intent.clone(),
            stage,
        });
    }
    effects.requirements.files.push(zup_plan::RequiredFile {
        key: zup_core::ResourceKey::File {
            id: zup_core::FileResourceId::Payload {
                id: zup_core::PayloadId::TreeMember {
                    declaration: zup_core::ResourceId::new("application").unwrap(),
                    member: path.clone(),
                },
            },
        },
        placement: zup_core::PayloadPlacement::Generation { path: path.clone() },
        size: contents.len() as u64,
        sha256: hash_bytes(contents),
        executable: false,
        privilege: Privilege::User,
    });
    effects.generations.push(GenerationIntent {
        sources: BTreeMap::from([(
            path.clone(),
            zup_plan::GenerationSource {
                key: zup_core::ResourceKey::File {
                    id: zup_core::FileResourceId::Payload {
                        id: zup_core::PayloadId::TreeMember {
                            declaration: zup_core::ResourceId::new("application").unwrap(),
                            member: path,
                        },
                    },
                },
                source_relative: RelativePath::new("build/asset.bin").unwrap(),
            },
        )]),
        preparation: GenerationPreparation::Materialize,
        materialization: MaterializationSpec::new(
            installation,
            id,
            directory
                .join(format!("{}/{id}", zup_core::DEPLOYMENTS_DIRECTORY))
                .unwrap(),
            descriptor,
        )
        .unwrap(),
        publication: zup_plan::GenerationPublication::Application {
            previous: effects.observed,
            revision: prior.map_or(ActivationRevision::FIRST, |record| {
                record.current.revision.next().unwrap()
            }),
        },
        privilege: Privilege::User,
        exposures: BTreeSet::new(),
    });
    let maintenance = MaterializationId::new_v7();
    let runtime = std::fs::read(r"C:\Windows\System32\PING.EXE").unwrap();
    std::fs::write(source.join("maintenance.bin"), &runtime).unwrap();
    let path = RelativePath::new("maintenance.exe").unwrap();
    effects.generations.push(GenerationIntent {
        sources: BTreeMap::from([(
            path.clone(),
            zup_plan::GenerationSource {
                key: zup_core::ResourceKey::File {
                    id: zup_core::FileResourceId::Maintenance {
                        artifact: zup_core::MaintenanceArtifact::Runtime,
                    },
                },
                source_relative: RelativePath::new("maintenance.bin").unwrap(),
            },
        )]),
        preparation: GenerationPreparation::Materialize,
        materialization: MaterializationSpec::new(
            installation,
            maintenance,
            directory
                .join(format!(
                    "{}/{maintenance}",
                    zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY
                ))
                .unwrap(),
            GenerationDescriptor::new(
                target,
                vec![GenerationFile {
                    path,
                    size: runtime.len() as u64,
                    sha256: hash_bytes(&runtime),
                    executable: true,
                }],
            )
            .unwrap(),
        )
        .unwrap(),
        publication: zup_plan::GenerationPublication::Maintenance,
        privilege: Privilege::User,
        exposures: BTreeSet::new(),
    });
    effects
}

fn commit_effects(state: &Path, source: &Path, effects: DeploymentEffects) -> InstallationRecord {
    let plan = compile_deployment(&effects).unwrap();
    let publisher = native_publisher(state);
    publisher
        .validate_plan(&plan.requirements.app.id, SelectedScope::User, &plan)
        .unwrap();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(state))
        .begin(
            AppId::new("example.generations").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan,
        )
        .unwrap();
    let mut executor = native_executor(state, source, open.record());
    let settled = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(
        settled.outcome,
        TransactionOutcome::Committed,
        "{:?}",
        settled.record
    );
    publisher
        .load(&settled.record.app_id, SelectedScope::User)
        .unwrap()
        .unwrap()
}

fn native_executor(
    state: &Path,
    source: &Path,
    record: &zup_transaction::TransactionRecord,
) -> Executor {
    Executor::new(state, source, record.transaction_id)
}

fn native_publisher(state: &Path) -> InstallationRecordStore {
    InstallationRecordStore::new(state)
}

#[test]
fn maintenance_generations_preserve_preset_contracts_and_retire_without_mutating_old_tools() {
    use zup_core::{
        InstalledPreset, MaintenanceArtifact, NonEmptyString, PresetAsset, PresetRuntime,
        ResourceKey,
    };
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let source = root.path().join("source");
    let state = root.path().join("state");
    let directory = TargetPath::new(
        Target::WINDOWS_X64,
        root.path().join("application").to_string_lossy(),
    )
    .unwrap();
    let installation = InstallationId::new_v7();
    let mut previous: Option<InstallationRecord> = None;
    let settings =
        serde_json::json!({"nested": {"array": [1,2,3], "enabled": true}, "title": "Acme"});
    for bytes in [b"first preset".as_slice(), b"second preset".as_slice()] {
        let ui = InstalledPreset {
            executable: hash_bytes(bytes),
            preset: PresetRuntime {
                name: NonEmptyString::new("aurora").unwrap(),
                version: "1.0.0".parse().unwrap(),
                protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
                required_capabilities: vec![],
                settings: settings.clone(),
                assets: vec![PresetAsset {
                    name: NonEmptyString::new("logo.svg").unwrap(),
                    size: 6,
                    sha256: hash_bytes(b"<svg/>"),
                }],
            },
        };
        let mut requirements = requirements_fixture::requirements(
            Target::WINDOWS_X64,
            SelectedScope::User,
            "example.generations",
            "1.0.0",
        );
        requirements.preset = Some(ui.clone());
        let mut effects = DeploymentEffects::new(installation, requirements);
        effects.install_directory = Some(directory.clone());
        effects.observed = previous.as_ref().map(|record| record.current.selection());
        effects.observed_transaction = previous.as_ref().map(|record| record.current.transaction);
        let id = MaterializationId::new_v7();
        let mut files = Vec::new();
        let mut sources = BTreeMap::new();
        for (artifact, contents) in [
            (
                MaintenanceArtifact::Runtime,
                b"maintenance runtime".as_slice(),
            ),
            (MaintenanceArtifact::Preset, bytes),
            (
                MaintenanceArtifact::Asset {
                    name: RelativePath::new("logo.svg").unwrap(),
                },
                b"<svg/>".as_slice(),
            ),
        ] {
            let path = artifact.member_path(Target::WINDOWS_X64, hash_bytes(contents));
            let native = source.join(path.as_str());
            std::fs::create_dir_all(native.parent().unwrap()).unwrap();
            std::fs::write(native, contents).unwrap();
            files.push(GenerationFile {
                path: path.clone(),
                size: contents.len() as u64,
                sha256: hash_bytes(contents),
                executable: !matches!(artifact, MaintenanceArtifact::Asset { .. }),
            });
            sources.insert(
                path.clone(),
                zup_plan::GenerationSource {
                    key: ResourceKey::File {
                        id: zup_core::FileResourceId::Maintenance { artifact },
                    },
                    source_relative: path,
                },
            );
        }
        effects.generations.push(GenerationIntent {
            materialization: MaterializationSpec::new(
                installation,
                id,
                directory
                    .join(format!(
                        "{}/{id}",
                        zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY
                    ))
                    .unwrap(),
                GenerationDescriptor::new(Target::WINDOWS_X64, files).unwrap(),
            )
            .unwrap(),
            sources,
            preparation: GenerationPreparation::Materialize,
            publication: zup_plan::GenerationPublication::Maintenance,
            privilege: Privilege::User,
            exposures: BTreeSet::new(),
        });
        let current = commit_effects(&state, &source, effects);
        assert_eq!(current.preset(), Some(&ui));
        assert_eq!(current.current.requirements.preset, Some(ui));
        assert!(current.current.materialization.is_none());
        assert_eq!(current.retained.len(), usize::from(previous.is_some()));
        let decoded = InstallationRecord::decode(&current.encode().unwrap()).unwrap();
        assert_eq!(decoded.preset().unwrap().preset.settings, settings);
        previous = Some(current);
    }
    let previous = previous.unwrap();
    let mut effects = DeploymentEffects::new(installation, previous.current.requirements.clone());
    effects.install_directory = Some(directory);
    effects.observed = Some(previous.current.selection());
    effects.observed_transaction = Some(previous.current.transaction);
    effects.transition = zup_plan::DeploymentTransition::Remove;
    effects.preset = None;
    let removed = commit_effects(&state, &source, effects);
    assert!(!removed.is_installed());
    assert!(removed.current.maintenance.is_none());
    assert_eq!(removed.retained.len(), 2);
    for snapshot in &removed.retained {
        let spec = snapshot.maintenance.as_ref().unwrap();
        for member in spec.descriptor().files() {
            let file = std::fs::File::open(
                Path::new(spec.directory().as_str()).join(member.path.as_str()),
            )
            .unwrap();
            assert_eq!(
                zup_core::hash_reader(file).unwrap(),
                (member.size, member.sha256)
            );
        }
    }
}

#[test]
fn upgrade_and_repair_create_verified_instances_without_mutating_prior_content() {
    let temporary =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"generation-a").unwrap();
    let a = deploy(&state, &application, &source, b"generation-a", None);
    let data = a.data.values().next().unwrap();
    let database =
        Path::new(data.directory.location.path().unwrap().as_str()).join("database.sqlite");
    std::fs::write(&database, b"application-owned mutable contents").unwrap();
    let instance_a = a.current.materialization.as_ref().unwrap();
    let file_a = Path::new(instance_a.directory().as_str()).join("lib/data.txt");
    assert_eq!(std::fs::read(&file_a).unwrap(), b"generation-a");
    std::fs::write(source.join("build/asset.bin"), b"generation-b").unwrap();
    let b = deploy(&state, &application, &source, b"generation-b", Some(&a));
    assert_eq!(b.data, a.data);
    assert_eq!(
        std::fs::read(&database).unwrap(),
        b"application-owned mutable contents"
    );
    let instance_b = b.current.materialization.as_ref().unwrap();
    let file_b = Path::new(instance_b.directory().as_str()).join("lib/data.txt");
    assert_eq!(std::fs::read(&file_a).unwrap(), b"generation-a");
    assert_eq!(std::fs::read(&file_b).unwrap(), b"generation-b");
    assert_eq!(b.retained, vec![a.current]);
    std::fs::write(&file_b, b"external-corruption").unwrap();
    let repaired = deploy(&state, &application, &source, b"generation-b", Some(&b));
    let replacement = repaired.current.materialization.as_ref().unwrap();
    assert_eq!(replacement.reference().tree, instance_b.reference().tree);
    assert_ne!(replacement.id(), instance_b.id());
    assert_eq!(
        std::fs::read(Path::new(replacement.directory().as_str()).join("lib/data.txt")).unwrap(),
        b"generation-b"
    );
    assert_eq!(std::fs::read(&file_b).unwrap(), b"external-corruption");
    assert_eq!(std::fs::read(&file_a).unwrap(), b"generation-a");
}

#[test]
fn matching_foreign_content_is_never_adopted_as_a_materialization() {
    let temporary =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let state = temporary.path().join("state");
    zup_storage::ensure_private_dir(&state).unwrap();
    let target = Target::WINDOWS_X64;
    let id = MaterializationId::new_v7();
    let application = TargetPath::new(
        target,
        temporary.path().join("application").to_string_lossy(),
    )
    .unwrap();
    let directory = application
        .join(format!("{}/{id}", zup_core::DEPLOYMENTS_DIRECTORY))
        .unwrap();
    let application_root = private_fs::PrivateDir::ensure(application.as_str()).unwrap();
    let mut namespace = application_root;
    for part in zup_core::DEPLOYMENTS_DIRECTORY.split('/') {
        namespace = namespace.create_dir(part).unwrap();
    }
    let _foreign_directory = namespace.create_dir(id.to_string()).unwrap();
    let foreign = Path::new(directory.as_str()).join("foreign.txt");
    std::fs::write(&foreign, b"matching").unwrap();
    let path = RelativePath::new("foreign.txt").unwrap();
    let intent = GenerationIntent {
        sources: BTreeMap::from([(
            path.clone(),
            zup_plan::GenerationSource {
                key: zup_core::ResourceKey::File {
                    id: zup_core::FileResourceId::Payload {
                        id: zup_core::PayloadId::TreeMember {
                            declaration: zup_core::ResourceId::new("application").unwrap(),
                            member: path.clone(),
                        },
                    },
                },
                source_relative: path.clone(),
            },
        )]),
        preparation: GenerationPreparation::Materialize,
        materialization: MaterializationSpec::new(
            InstallationId::new_v7(),
            id,
            directory,
            GenerationDescriptor::new(
                target,
                vec![GenerationFile {
                    path,
                    size: 8,
                    sha256: hash_bytes(b"matching"),
                    executable: false,
                }],
            )
            .unwrap(),
        )
        .unwrap(),
        publication: zup_plan::GenerationPublication::Application {
            previous: None,
            revision: ActivationRevision::FIRST,
        },
        privilege: Privilege::User,
        exposures: BTreeSet::new(),
    };
    let installation = intent.materialization.installation();
    let mut effects = DeploymentEffects::new(
        installation,
        requirements_fixture::requirements(
            target,
            zup_core::SelectedScope::User,
            "example.foreign-generation",
            "1.0.0",
        ),
    );
    effects.install_directory = Some(application);
    effects.generations.push(intent);
    let plan = compile_deployment(&effects).unwrap();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
        .begin(
            AppId::new("example.foreign-generation").unwrap(),
            SelectedScope::User,
            "1.0.0".parse().unwrap(),
            plan.clone(),
        )
        .unwrap();
    let mut executor = Executor::new(&state, temporary.path(), open.record().transaction_id);
    let settled = open
        .execute(&mut executor, &InstallationRecordStore::new(&state))
        .unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RecoveryRequired);
    assert!(
        !state
            .join("materializations")
            .join(installation.to_string())
            .join(format!("{id}.json"))
            .exists()
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), b"matching");
    assert_eq!(
        std::fs::read_dir(foreign.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[cfg(feature = "test-faults")]
#[rstest::rstest]
#[case("apply_returned", "generation:materialize:")]
#[case("apply_returned", "generation:seal:")]
#[case("apply_returned", "generation:activate:")]
#[case("apply_returned", "data_prepare:")]
#[case("persistent_creation_requested", "data_prepare:")]
#[case("persistent_creation_recorded", "data_prepare:")]
#[case("persistent_directory_created", "data_prepare:")]
#[case("materialization_directory_created", "generation:materialize:")]
#[case("materialization_preparation_recorded", "generation:materialize:")]
#[case("materialization_publication_requested", "generation:seal:")]
#[case("materialization_access_published", "generation:seal:")]
#[case("materialization_directory_published", "generation:seal:")]
#[case("materialization_stream_interrupted", "generation:materialize:")]
fn native_generation_crash_recovers_without_a_completion_receipt(
    #[case] point: &str,
    #[case] boundary: &str,
) {
    const CHILD_ROOT: &str = "ZUP_GENERATION_CRASH_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = std::path::PathBuf::from(root);
        let source = root.join("source");
        std::fs::create_dir_all(source.join("build")).unwrap();
        std::fs::write(source.join("build/asset.bin"), b"recoverable").unwrap();
        deploy(
            &root.join("state"),
            &root.join("application"),
            &source,
            b"recoverable",
            None,
        );
        panic!("fault injection must terminate the child");
    }
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let thread = std::thread::current();
    let test = thread.name().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .env("ZUP_TEST_FAULT_POINT", point)
        .env("ZUP_TEST_FAULT_OPERATION", boundary)
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    assert!(
        !child.status.success(),
        "fault injection must terminate the child"
    );
    use zup_transaction::TransactionStore;
    let state = root.path().join("state");
    let store = FilesystemTransactionStore::private_at(&state);
    let ids = store.list_ids().unwrap();
    assert_eq!(ids.len(), 1);
    let pending = store.load(&ids[0]).unwrap();
    assert!(
        pending
            .plan
            .nodes
            .iter()
            .any(|node| node.id.as_str().starts_with(boundary)
                && matches!(
                    pending.nodes.get(&node.id),
                    Some(zup_transaction::NodeState::Running)
                ))
    );
    let mut executor = Executor::new(&state, &root.path().join("source"), pending.transaction_id);
    let publisher = InstallationRecordStore::new(&state);
    let (committed, outcome) =
        zup_transaction::recover(pending, &store, &mut executor, &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed, "{:?}", committed);
    let installed = publisher
        .load(&committed.app_id, SelectedScope::User)
        .unwrap()
        .unwrap();
    let selected = installed.current.materialization.as_ref().unwrap();
    assert_eq!(
        std::fs::read(Path::new(selected.directory().as_str()).join("lib/data.txt")).unwrap(),
        b"recoverable"
    );
    if point == "materialization_stream_interrupted" {
        let record_path = state
            .join("materializations")
            .join(selected.installation().to_string())
            .join(format!("{}.json", selected.id()));
        let metadata: zup_transaction::MaterializationRecord =
            serde_json::from_slice(&std::fs::read(record_path).unwrap()).unwrap();
        assert_eq!(metadata.retired_preparations.len(), 1);
        let interrupted = Path::new(selected.directory().as_str())
            .parent()
            .unwrap()
            .join(metadata.retired_preparations[0].directory.as_str())
            .join("lib");
        let entries: Vec<_> = std::fs::read_dir(interrupted)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(std::fs::read(entries[0].path()).unwrap(), b"reco");
    }
}

struct Executor(WindowsFileExecutor<interrupted_payload::InterruptedPayload>);

impl Executor {
    fn new(state: &Path, source: &Path, transaction: zup_core::TransactionId) -> Self {
        Self(
            WindowsFileExecutor::new(
                interrupted_payload::InterruptedPayload(zup_bundle::DirectoryPayloadSource::new(
                    source,
                )),
                transaction,
                ".exe",
            )
            .with_state_authority(
                state.to_owned(),
                SelectedScope::User,
                &FilesystemTransactionStore::private_at(state)
                    .load(&transaction)
                    .unwrap()
                    .app_id,
            ),
        )
    }
}

impl zup_transaction::OperationExecutor for Executor {
    type Error = String;
    fn prepare(&mut self, _: &zup_plan::DeploymentNode) -> Result<(), String> {
        Ok(())
    }
    fn apply(
        &mut self,
        node: &zup_plan::DeploymentNode,
        _transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::OperationReceipt, String> {
        if matches!(node.kind, zup_plan::NodeKind::Barrier) {
            Ok(zup_transaction::OperationReceipt::Control)
        } else if matches!(node.kind, zup_plan::NodeKind::Cleanup { .. }) {
            self.0.apply_cleanup(node, _transaction)
        } else if matches!(node.kind, zup_plan::NodeKind::PersistentDirectory { .. }) {
            self.0.apply_persistent(node, _transaction)
        } else if matches!(node.kind, zup_plan::NodeKind::Generation { .. }) {
            self.0.apply_generation(node)
        } else {
            zup_windows::apply_node(&mut self.0, node, _transaction)
                .map_err(|error| error.to_string())
        }
    }
    fn verify(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        match receipt {
            zup_transaction::OperationReceipt::Cleanup { .. } => {
                self.0.verify_cleanup(node, receipt)
            }
            zup_transaction::OperationReceipt::PersistentDirectory(owned) => {
                self.0.verify_persistent(node, owned)
            }
            zup_transaction::OperationReceipt::Generation(receipt) => {
                self.0.verify_generation(node, receipt)
            }
            zup_transaction::OperationReceipt::Control => Ok(()),
            _ => {
                zup_windows::verify_installed_file(node, receipt).map_err(|error| error.to_string())
            }
        }
    }
    fn rollback(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: &zup_transaction::OperationReceipt,
    ) -> Result<(), String> {
        match receipt {
            zup_transaction::OperationReceipt::PersistentDirectory(owned) => {
                self.0.verify_persistent(node, owned)
            }
            zup_transaction::OperationReceipt::Generation(receipt) => {
                self.0.retain_generation(node, receipt)
            }
            zup_transaction::OperationReceipt::Control => Ok(()),
            _ => self
                .0
                .rollback_transaction_receipt(node, receipt)
                .map_err(|error| error.to_string()),
        }
    }
    fn reconcile(
        &mut self,
        node: &zup_plan::DeploymentNode,
        receipt: Option<&zup_transaction::OperationReceipt>,
        _transaction: &zup_transaction::TransactionRecord,
    ) -> Result<zup_transaction::ReconcileResult, String> {
        if matches!(node.kind, zup_plan::NodeKind::Cleanup { .. }) {
            return self
                .0
                .apply_cleanup(node, _transaction)
                .map(zup_transaction::ReconcileResult::AppliedWithReceipt);
        }
        if matches!(node.kind, zup_plan::NodeKind::Barrier) {
            Ok(zup_transaction::ReconcileResult::Applied)
        } else if matches!(node.kind, zup_plan::NodeKind::PersistentDirectory { .. }) {
            self.0
                .reconcile_transaction_node(node, receipt, _transaction)
                .map_err(|error| error.to_string())
        } else if matches!(node.kind, zup_plan::NodeKind::Generation { .. }) {
            self.0.reconcile_generation(node, receipt)
        } else {
            self.0
                .reconcile_transaction_node(node, receipt, _transaction)
                .map_err(|error| error.to_string())
        }
    }
}

#[derive(Clone, Copy)]
enum TreeMutation {
    ExtraFile,
    Junction,
    AlternateStream,
    HardLink,
}

#[rstest::rstest]
#[case(TreeMutation::ExtraFile)]
#[case(TreeMutation::Junction)]
#[case(TreeMutation::AlternateStream)]
#[case(TreeMutation::HardLink)]
fn sealed_tree_rejects_unverified_filesystem_state(#[case] mutation: TreeMutation) {
    let root =
        tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"original").unwrap();
    let state = root.path().join("state");
    let installed = deploy(
        &state,
        &root.path().join("application"),
        &source,
        b"original",
        None,
    );
    let generation = Path::new(
        installed
            .current
            .materialization
            .as_ref()
            .unwrap()
            .directory()
            .as_str(),
    );
    if matches!(mutation, TreeMutation::Junction) {
        let foreign = root.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("data.txt"), b"original").unwrap();
        std::fs::rename(generation.join("lib"), generation.join("previous-lib")).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(generation.join("lib"))
            .arg(&foreign)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Remove the moved directory so the rejection cannot depend on an extra tree entry.
        std::fs::remove_dir_all(generation.join("previous-lib")).unwrap();
    } else {
        match mutation {
            TreeMutation::ExtraFile => {
                std::fs::write(generation.join("undeclared.txt"), b"unexpected").unwrap();
            }
            TreeMutation::AlternateStream => {
                let file = generation.join("lib/data.txt");
                let readonly = std::fs::metadata(&file).unwrap().permissions();
                let mut writable = readonly.clone();
                #[expect(
                    clippy::permissions_set_readonly_false,
                    reason = "Windows read-only attribute; ACLs are unchanged"
                )]
                writable.set_readonly(false);
                std::fs::set_permissions(&file, writable).unwrap();
                std::fs::write(generation.join("lib/data.txt:unverified"), b"unexpected").unwrap();
                std::fs::set_permissions(&file, readonly).unwrap();
            }
            TreeMutation::HardLink => {
                std::fs::hard_link(
                    generation.join("lib/data.txt"),
                    root.path().join("foreign-link"),
                )
                .unwrap();
            }
            TreeMutation::Junction => unreachable!(),
        }
    }
    use zup_transaction::{OperationExecutor, TransactionStore};
    let transaction = FilesystemTransactionStore::private_at(&state)
        .load(&installed.current.transaction)
        .unwrap();
    let mut executor = Executor::new(&state, &source, transaction.transaction_id);
    let node = transaction
        .plan
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.kind,
                zup_plan::NodeKind::Generation {
                    operation: zup_plan::GenerationOperation::Activate,
                    ..
                }
            )
        })
        .unwrap();
    assert!(
        executor
            .verify(node, transaction.receipt(&node.id).unwrap())
            .is_err()
    );
    if matches!(mutation, TreeMutation::Junction) {
        assert_eq!(
            std::fs::read(root.path().join("foreign/data.txt")).unwrap(),
            b"original"
        );
    }
}

mod cleanup_lifecycle {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/cleanup_lifecycle.rs"
    ));
}
