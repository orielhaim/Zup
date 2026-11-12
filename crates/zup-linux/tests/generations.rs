#![cfg(target_os = "linux")]

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
#[cfg(feature = "test-faults")]
mod preparation_retirement {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/preparation_retirement.rs"
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
use zup_linux::{LinuxFileExecutor, LinuxInstallationStore};
use zup_plan::{
    DeploymentEffects, GenerationIntent, GenerationPreparation, InstallationRecord,
    compile_deployment,
};
use zup_transaction::{FilesystemTransactionStore, TransactionCoordinator, TransactionOutcome};

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
    _source: &Path,
    contents: &[u8],
    prior: Option<&InstallationRecord>,
    claims: Option<&zup_plan::PersistentClaimSelection>,
) -> DeploymentEffects {
    let target = Target::LINUX_X64;
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
    effects
}

fn commit_effects(state: &Path, source: &Path, effects: DeploymentEffects) -> InstallationRecord {
    let scope = effects.requirements.scope;
    let plan = compile_deployment(&effects).unwrap();
    let publisher = native_publisher(state);
    publisher
        .validate_plan(&plan.requirements.app.id, scope, &plan)
        .unwrap();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(state))
        .begin(
            AppId::new("example.generations").unwrap(),
            scope,
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
        .load(&settled.record.app_id, scope)
        .unwrap()
        .unwrap()
}

fn native_executor(
    state: &Path,
    source: &Path,
    record: &zup_transaction::TransactionRecord,
) -> LinuxFileExecutor {
    let mut executor = LinuxFileExecutor::new().with_payload(
        interrupted_payload::InterruptedPayload(zup_bundle::DirectoryPayloadSource::new(source)),
    );
    executor.set_state_authority(state.to_owned(), record.app_id.clone(), record.scope);
    executor.register_plan(&record.plan).unwrap();
    executor.set_transaction_id(record.transaction_id);
    executor
}

fn native_publisher(state: &Path) -> LinuxInstallationStore {
    LinuxInstallationStore::new(state)
}

#[cfg(feature = "test-faults")]
#[path = "generations/access_publication.rs"]
mod access_publication;

#[cfg(feature = "test-faults")]
#[path = "generations/consumer_access.rs"]
mod consumer_access;

#[test]
fn upgrade_and_repair_create_verified_instances_without_mutating_prior_content() {
    let temporary = tempfile::tempdir().unwrap();
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
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&file_b, std::fs::Permissions::from_mode(0o644)).unwrap();
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
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    zup_storage::ensure_private_dir(&state).unwrap();
    let target = Target::LINUX_X64;
    let id = MaterializationId::new_v7();
    let application = TargetPath::new(
        target,
        temporary.path().join("application").to_string_lossy(),
    )
    .unwrap();
    let directory = application
        .join(format!("{}/{id}", zup_core::DEPLOYMENTS_DIRECTORY))
        .unwrap();
    std::fs::create_dir_all(directory.as_str()).unwrap();
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
    let mut executor = LinuxFileExecutor::new()
        .with_payload(zup_bundle::DirectoryPayloadSource::new(temporary.path()));
    executor.set_state_authority(
        state.clone(),
        open.record().app_id.clone(),
        open.record().scope,
    );
    executor.set_transaction_id(open.record().transaction_id);
    executor.register_plan(&plan).unwrap();
    let settled = open
        .execute(&mut executor, &LinuxInstallationStore::new(&state))
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
    let root = tempfile::tempdir().unwrap();
    let thread = std::thread::current();
    let test = thread.name().unwrap();
    let child = std::process::Command::new("sh")
        .args(["-c", "ulimit -c 0; exec \"$@\"", "zup-generation-crash"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .env("ZUP_TEST_FAULT_POINT", point)
        .env("ZUP_TEST_FAULT_OPERATION", boundary)
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        child.status.signal(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&child.stderr)
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
    let mut executor =
        LinuxFileExecutor::new().with_payload(interrupted_payload::InterruptedPayload(
            zup_bundle::DirectoryPayloadSource::new(root.path().join("source")),
        ));
    executor.set_state_authority(state.clone(), pending.app_id.clone(), pending.scope);
    executor.set_transaction_id(pending.transaction_id);
    executor.register_plan(&pending.plan).unwrap();
    let publisher = LinuxInstallationStore::new(&state);
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
        assert!(metadata.retired_preparations.is_empty());
        let namespace = Path::new(selected.directory().as_str()).parent().unwrap();
        assert!(std::fs::read_dir(namespace).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".preparing-")
        }));
    }
}

#[test]
fn manifest_pipeline_installs_and_upgrades_mixed_placement_through_one_graph() {
    let target = Target::host().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    let state = temporary.path().join("state");
    std::fs::create_dir_all(&source).unwrap();
    let executable = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .unwrap();
    std::fs::copy(executable, source.join("native-main")).unwrap();
    std::fs::write(source.join("settings.default"), b"settings").unwrap();
    let text = format!(
        r#"
schema = 1
[app]
id = "example.hybrid"
name = "Hybrid"
version = "1.0.0"
main = {{ kind = "file", declaration = "program" }}
[build.targets.linux]
target = "{target}"
source = {{ directory = "source" }}
frontend = "headless"
[install]
scope = "user"
[install.directory]
user = "{}"
[[files]]
id = "application"
source = "application.txt"
[[files]]
id = "program"
source = "native-main"
placement = {{ kind = "generation_file", path = "bin/true" }}
[[files]]
id = "settings"
source = "settings.default"
placement = {{ kind = "fixed", directory = "${{install}}" }}
"#,
        application.display()
    );
    let manifest_path = temporary.path().join("zup.toml");
    std::fs::write(&manifest_path, &text).unwrap();
    let manifest = zup_manifest::parse(&text).unwrap();
    let overrides = zup_core::TargetOverrides::default();
    let configs = zup_manifest::select_targets(&manifest, &["linux"], &overrides).unwrap();
    let publisher = LinuxInstallationStore::new(&state);
    let mut previous: Option<InstallationRecord> = None;
    let reuse = zup_plan::GenerationPolicy::ReuseVerified;
    let reconstruct = zup_plan::GenerationPolicy::Reconstruct;
    for (contents, settings, policy) in [
        (b"generation-a".as_slice(), b"settings-1".as_slice(), reuse),
        (b"generation-b".as_slice(), b"settings-1".as_slice(), reuse),
        (b"generation-b".as_slice(), b"settings-2".as_slice(), reuse),
        (
            b"generation-b".as_slice(),
            b"settings-2".as_slice(),
            reconstruct,
        ),
    ] {
        std::fs::write(source.join("application.txt"), contents).unwrap();
        std::fs::write(source.join("settings.default"), settings).unwrap();
        let selected: Vec<_> = configs
            .iter()
            .map(|config| {
                (
                    config.clone(),
                    zup_manifest::compile(&manifest, config, &overrides).unwrap(),
                )
            })
            .collect();
        let build =
            zup_build::materialize(&manifest_path, &manifest, selected, zup_build::Writes::None)
                .unwrap();
        let mut request = zup_plan::PlanRequest::new(target, SelectedScope::User);
        request.generation_policy = policy;
        if let Some(installed) = &previous {
            request.installation = installed.id;
            request.previous = Some(installed.current.selection());
        }
        let desired = zup_plan::plan(&build, &request, None, &zup_plan::NeverCancelled)
            .unwrap()
            .plan;
        let desired = zup_linux::resolve_target(
            &desired,
            &zup_linux::LinuxInstallLocationResolver::default(),
        )
        .unwrap();
        let snapshot = zup_linux::snapshot_target(&desired);
        assert_eq!(snapshot.files.len(), 1);
        let execution = zup_plan::plan_execution(&desired, &snapshot, previous.as_ref()).unwrap();
        let effects = zup_linux::compile_execution_plan(&execution, Some(&desired)).unwrap();
        let graph = compile_deployment(&effects).unwrap();
        let selected_deployment = zup_plan::DeploymentSelection::from_plan(&graph);
        selected_deployment.validate(previous.as_ref()).unwrap();
        let mut authority_request = zup_plan::PlanRequest::new(target, SelectedScope::User);
        selected_deployment.apply_to(&mut authority_request);
        let authority_desired =
            zup_plan::plan(&build, &authority_request, None, &zup_plan::NeverCancelled)
                .unwrap()
                .plan;
        let authority_desired = zup_linux::resolve_target(
            &authority_desired,
            &zup_linux::LinuxInstallLocationResolver::default(),
        )
        .unwrap();
        let authority_delta =
            zup_plan::plan_execution(&authority_desired, &snapshot, previous.as_ref()).unwrap();
        let authority_effects =
            zup_linux::compile_execution_plan(&authority_delta, Some(&authority_desired)).unwrap();
        assert_eq!(compile_deployment(&authority_effects).unwrap(), graph);
        publisher
            .validate_plan(&desired.app.id, desired.scope, &graph)
            .unwrap();
        let mut executor =
            LinuxFileExecutor::new().with_payload(zup_bundle::DirectoryPayloadSource::new(&source));
        executor.set_state_authority(state.clone(), desired.app.id.clone(), desired.scope);
        executor.register_plan(&graph).unwrap();
        let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
            .begin(
                desired.app.id.clone(),
                desired.scope,
                desired.app.version.clone(),
                graph,
            )
            .unwrap();
        executor.set_transaction_id(open.record().transaction_id);
        let settled = open.execute(&mut executor, &publisher).unwrap();
        assert_eq!(
            settled.outcome,
            TransactionOutcome::Committed,
            "{:?}",
            settled.record
        );
        let installed = publisher
            .load(&desired.app.id, desired.scope)
            .unwrap()
            .unwrap();
        let generation = installed.current.materialization.as_ref().unwrap();
        assert!(
            std::process::Command::new(Path::new(generation.directory().as_str()).join("bin/true"))
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            std::fs::read(Path::new(generation.directory().as_str()).join("application.txt"))
                .unwrap(),
            contents
        );
        assert_eq!(
            std::fs::read(application.join("settings.default")).unwrap(),
            settings
        );
        assert!(installed.current.resources.keys().all(|key| matches!(
            key,
            zup_core::ResourceKey::File {
                id: zup_core::FileResourceId::Payload { .. }
            }
        )));
        if let Some(prior) = &previous {
            let old = prior.current.materialization.as_ref().unwrap();
            if old.reference().tree == generation.reference().tree && policy == reuse {
                assert_eq!(old.id(), generation.id());
            } else {
                assert_ne!(old.id(), generation.id());
            }
        }
        previous = Some(installed);
    }
    let installed = previous.unwrap();
    let retained = installed.retained[0].materialization.as_ref().unwrap();
    assert_eq!(
        std::fs::read(Path::new(retained.directory().as_str()).join("application.txt")).unwrap(),
        b"generation-a"
    );
}

#[test]
fn canonical_retained_hybrid_rollback_uses_fixed_instances_without_source_files() {
    let temporary = tempfile::tempdir().unwrap();
    let target = Target::host().unwrap();
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    let state = temporary.path().join("state");
    std::fs::create_dir(&source).unwrap();
    let manifest_path = temporary.path().join("zup.toml");
    let mut original = None;
    let mut installed: Option<InstallationRecord> = None;
    let mut latest = None;
    let publisher = LinuxInstallationStore::new(&state);
    for (version, bytes) in [
        ("1.0.0", b"release-a".as_slice()),
        ("2.0.0", b"release-b".as_slice()),
    ] {
        std::fs::write(source.join("application.txt"), bytes).unwrap();
        std::fs::write(source.join("fixed.txt"), bytes).unwrap();
        let text = format!(
            r#"
schema = 1
[app]
id = "example.retained-hybrid"
name = "Retained hybrid"
version = "{version}"
[build.targets.linux]
target = "{target}"
source = {{ directory = "source" }}
frontend = "headless"
[install]
scope = "user"
[install.directory]
user = "{}"
[[files]]
id = "application"
source = "application.txt"
[[files]]
id = "fixed"
source = "fixed.txt"
placement = {{ kind = "fixed", directory = "${{install}}" }}
"#,
            application.display()
        );
        std::fs::write(&manifest_path, &text).unwrap();
        let manifest = zup_manifest::parse(&text).unwrap();
        let overrides = zup_core::TargetOverrides::default();
        let configs = zup_manifest::select_targets(&manifest, &["linux"], &overrides).unwrap();
        let selected: Vec<_> = configs
            .into_iter()
            .map(|config| {
                let installer = zup_manifest::compile(&manifest, &config, &overrides).unwrap();
                (config, installer)
            })
            .collect();
        let build =
            zup_build::materialize(&manifest_path, &manifest, selected, zup_build::Writes::None)
                .unwrap();
        let mut request = zup_plan::PlanRequest::new(target, SelectedScope::User);
        if let Some(previous) = &installed {
            request.installation = previous.id;
            request.previous = Some(previous.current.selection());
        }
        let desired = zup_plan::plan(&build, &request, None, &zup_plan::NeverCancelled)
            .unwrap()
            .plan;
        let desired = zup_linux::resolve_target(
            &desired,
            &zup_linux::LinuxInstallLocationResolver::default(),
        )
        .unwrap();
        let snapshot = zup_linux::snapshot_target(&desired);
        let execution = zup_plan::plan_execution(&desired, &snapshot, installed.as_ref()).unwrap();
        let effects = zup_linux::compile_execution_plan(&execution, Some(&desired)).unwrap();
        let graph = compile_deployment(&effects).unwrap();
        publisher
            .validate_plan(&desired.app.id, desired.scope, &graph)
            .unwrap();
        let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
            .begin(
                desired.app.id.clone(),
                desired.scope,
                desired.app.version.clone(),
                graph,
            )
            .unwrap();
        let mut executor = native_executor(&state, &source, open.record());
        let settled = open.execute(&mut executor, &publisher).unwrap();
        assert_eq!(settled.outcome, TransactionOutcome::Committed);
        publisher
            .reconcile_installation(&settled.record, SelectedScope::User)
            .unwrap();
        installed = publisher.load(&desired.app.id, desired.scope).unwrap();
        if original.is_none() {
            original = installed.clone();
        }
        latest = Some(build.targets[0].clone());
    }
    let original = original.unwrap();
    let installed = installed.unwrap();
    std::fs::remove_file(source.join("application.txt")).unwrap();
    std::fs::remove_file(source.join("fixed.txt")).unwrap();
    let mut build = latest.unwrap();
    build.installer.app.version = "99.0.0".parse().unwrap();
    let session = zup_linux::LinuxSessionHost::open(source.join("absent-installer"));
    let context = zup_session::OperationContext {
        scope: SelectedScope::User,
        state_root: state.clone(),
        ledger: Some(installed.clone()),
    };
    let host = zup_session::SessionHost::bind(&session, &context).unwrap();
    let prepared = zup_runtime::prepare_operation(
        &zup_runtime::OperationInput {
            build: &build,
            installer: &source.join("absent-installer"),
            scope: SelectedScope::User,
            operation: zup_runtime::Operation::Rollback {
                revision: original.current.revision,
            },
            selection: Default::default(),
            prior: Some(&installed),
            state_root: &state,
            cancellation: &zup_plan::NeverCancelled,
        },
        &host,
    )
    .unwrap();
    let request = zup_runtime::runtime_request(&prepared, state.clone(), Default::default());
    assert_eq!(request.app_version, original.current.version);
    let graph = prepared.transaction_plan;
    publisher
        .validate_plan(&installed.app_id, installed.scope, &graph)
        .unwrap();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
        .begin(
            installed.app_id.clone(),
            installed.scope,
            request.app_version,
            graph,
        )
        .unwrap();
    let mut executor = native_executor(&state, &source, open.record());
    let settled = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(
        settled.outcome,
        TransactionOutcome::Committed,
        "{:?}",
        settled.record.failure
    );
    let restored = publisher
        .load(&installed.app_id, installed.scope)
        .unwrap()
        .unwrap();
    assert_eq!(
        restored.current.materialization,
        original.current.materialization
    );
    assert_eq!(restored.current.requirements, original.current.requirements);
    assert_eq!(
        std::fs::read(application.join("fixed.txt")).unwrap(),
        b"release-a"
    );
}

mod cleanup_lifecycle {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/cleanup_lifecycle.rs"
    ));
}
