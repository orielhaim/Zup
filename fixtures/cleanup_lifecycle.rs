fn native_effects(application: &Path, source: &Path, contents: &[u8], previous: Option<&InstallationRecord>) -> DeploymentEffects {
    let mut effects = deployment_effects(application, source, contents, previous, None);
    effects.generations.retain(|generation| matches!(generation.publication, zup_plan::GenerationPublication::Application { .. }));
    effects.requirements.generation_readers = zup_core::GenerationReaders::NativeExecutables;
    let image = source.join("build/runner.bin");
    let bytes = std::fs::read(&image).unwrap();
    let path = RelativePath::new(if cfg!(windows) { "bin/runner.exe" } else { "bin/sleep" }).unwrap();
    let key = zup_core::ResourceKey::File { id: zup_core::FileResourceId::Payload { id: zup_core::PayloadId::File { declaration: zup_core::ResourceId::new("runner").unwrap() } } };
    effects.requirements.files.push(zup_plan::RequiredFile { key: key.clone(), placement: zup_core::PayloadPlacement::Generation { path: path.clone() }, size: bytes.len() as u64, sha256: hash_bytes(&bytes), executable: true, privilege: Privilege::User });
    effects.requirements.files.sort_by(|a, b| a.key.cmp(&b.key));
    let generation = &mut effects.generations[0];
    let mut files = generation.materialization.descriptor().files().to_vec();
    files.push(GenerationFile { path: path.clone(), size: bytes.len() as u64, sha256: hash_bytes(&bytes), executable: true });
    generation.materialization = MaterializationSpec::new(generation.materialization.installation(), generation.materialization.id(), generation.materialization.directory().clone(), GenerationDescriptor::new(effects.requirements.target, files).unwrap()).unwrap();
    generation.sources.insert(path, zup_plan::GenerationSource { key, source_relative: RelativePath::new("build/runner.bin").unwrap() });
    effects
}

fn maintain(state: &Path, source: &Path, previous: &InstallationRecord, retirement: Option<zup_plan::RetentionChange>) -> (zup_transaction::TransactionRecord, TransactionOutcome) {
    let mut effects = DeploymentEffects::new(previous.id, previous.current.requirements.clone());
    effects.transition = zup_plan::DeploymentTransition::Maintain;
    effects.observed = Some(previous.current.selection());
    effects.observed_transaction = Some(previous.commit.transaction);
    effects.install_directory = Some(previous.directory.clone());
    effects.retention = retirement;
    let remaining = zup_transaction::cleanup_candidates(&FilesystemTransactionStore::private_at(state), previous, None).unwrap();
    let forget = zup_transaction::collected_deployments(previous, &remaining);
    if !forget.is_empty() { effects.retention.get_or_insert_with(|| zup_plan::RetentionChange { retire: Vec::new(), forget: Vec::new() }).forget = forget; }

    effects.cleanup = zup_transaction::cleanup_candidates(&FilesystemTransactionStore::private_at(state), previous, effects.retention.as_ref()).unwrap();
    assert!(!effects.cleanup.is_empty() || effects.retention.is_some());
    let plan = compile_deployment(&effects).unwrap();
    let publisher = native_publisher(state);
    publisher.validate_plan(&plan.requirements.app.id, SelectedScope::User, &plan).unwrap();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(state)).begin(previous.app_id.clone(), previous.scope, previous.current.version.clone(), plan).unwrap();
    let mut executor = native_executor(state, source, open.record());
    let settled = open.execute(&mut executor, &publisher).unwrap();
    (settled.record, settled.outcome)
}

#[rstest::rstest]
#[case::collect(false, false)]
#[case::running_image(true, false)]
#[case::foreign_member(false, true)]
#[case::identical_substitution_while_running(true, true)]
fn generation_collection_preserves_live_consumers_and_foreign_objects(#[case] running: bool, #[case] substitute: bool) {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    #[cfg(windows)]
    let image = Path::new(r"C:\Windows\System32\PING.EXE");
    #[cfg(not(windows))]
    let image = Path::new("/usr/bin/sleep");
    std::fs::copy(image, source.join("build/runner.bin")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"first").unwrap();
    let first = commit_effects(&state, &source, native_effects(&application, &source, b"first", None));
    let old = Path::new(first.current.materialization.as_ref().unwrap().directory().as_str());
    let mut process = if running {
        let mut command = std::process::Command::new(old.join(if cfg!(windows) { "bin/runner.exe" } else { "bin/sleep" }));
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; command.args(["-n", "60", "127.0.0.1"]).creation_flags(0x08000000); }
        #[cfg(not(windows))]
        { use std::os::unix::process::CommandExt; command.arg0("sleep").arg("60"); }
        Some(command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::inherit()).spawn().unwrap())
    } else { None };
    std::fs::write(source.join("build/asset.bin"), b"second").unwrap();
    let second = commit_effects(&state, &source, native_effects(&application, &source, b"second", Some(&first)));
    let data = second.data.values().next().unwrap().ownership.binding().path(second.target).unwrap();
    std::fs::write(Path::new(data.as_str()).join("user-data"), b"keep").unwrap();
    if substitute {
        let path = old.join("lib/data.txt");
        #[cfg(not(windows))]
        { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap(); }
        if running {
            let original = old.join("lib/original-data");
            std::fs::rename(&path, &original).unwrap();
            std::fs::copy(&original, &path).unwrap();
            std::fs::remove_file(&original).unwrap();
        } else {
            std::fs::write(&path, b"foreign").unwrap();
        }
    }
    if let Some(process) = &mut process { let status = process.try_wait().unwrap(); assert!(status.is_none(), "native image exited before collection: {status:?}"); }
    let (record, outcome) = maintain(&state, &source, &second, zup_plan::RetentionChange::keeping_previous(&second, 0));
    if substitute {
        assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
        assert_eq!(record.phase, zup_transaction::TransactionPhase::Committed);
        assert_eq!(std::fs::read(old.join("lib/data.txt")).unwrap(), if running { b"first".as_slice() } else { b"foreign".as_slice() });
        if let Some(process) = &mut process {
            assert!(process.try_wait().unwrap().is_none());
            process.kill().unwrap();
            process.wait().unwrap();
        }
    } else {
        assert_eq!(outcome, TransactionOutcome::Committed, "{:?}", record.recovery_failure);
        let completion = record.plan.nodes.iter().find_map(|node| match record.receipt(&node.id) { Some(zup_transaction::OperationReceipt::Cleanup { completion, .. }) => Some(*completion), _ => None }).unwrap();
        if running {
            assert_eq!(completion, zup_transaction::CleanupCompletion::Deferred { reason: zup_transaction::CleanupDeferral::InUse });
            assert!(old.join("lib/data.txt").exists());
            let process = process.as_mut().unwrap();
            process.kill().unwrap(); process.wait().unwrap();
            let installation = native_publisher(&state).load(&second.app_id, SelectedScope::User).unwrap().unwrap();
            let (_, outcome) = maintain(&state, &source, &installation, None);
            assert_eq!(outcome, TransactionOutcome::Committed);
        } else { assert_eq!(completion, zup_transaction::CleanupCompletion::Removed); }
        assert!(!old.exists());
    }
    let installation = native_publisher(&state).load(&second.app_id, SelectedScope::User).unwrap().unwrap();
    assert_eq!(installation.current, second.current);
    assert_eq!(std::fs::read(Path::new(data.as_str()).join("user-data")).unwrap(), b"keep");
    assert!(Path::new(second.current.materialization.as_ref().unwrap().directory().as_str()).join("lib/data.txt").exists());
}

#[cfg(feature = "test-faults")]
#[test]
fn collection_fault_child() {
    let Some(root) = std::env::var_os("ZUP_COLLECTION_FIXTURE") else { return; };
    let root = Path::new(&root);
    let state = root.join("state");
    let installation = native_publisher(&state).load(&AppId::new("example.generations").unwrap(), SelectedScope::User).unwrap().unwrap();
    maintain(&state, &root.join("source"), &installation, zup_plan::RetentionChange::keeping_previous(&installation, 0));
    panic!("requested collection fault did not fire");
}

#[cfg(feature = "test-faults")]
#[rstest::rstest]
#[case("collection_prepared")]
#[case("collection_member_removed")]
#[cfg_attr(target_os = "linux", case("collection_image_writable"))]
fn interrupted_native_collection_resumes_without_reverting_the_activation(#[case] point: &str) {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    #[cfg(windows)]
    std::fs::copy(r"C:\Windows\System32\PING.EXE", source.join("build/runner.bin")).unwrap();
    #[cfg(not(windows))]
    std::fs::copy("/usr/bin/sleep", source.join("build/runner.bin")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"first").unwrap();
    let first = commit_effects(&state, &source, native_effects(&application, &source, b"first", None));
    std::fs::write(source.join("build/asset.bin"), b"second").unwrap();
    let second = commit_effects(&state, &source, native_effects(&application, &source, b"second", Some(&first)));
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cleanup_lifecycle::collection_fault_child", "--nocapture"])
        .env("ZUP_COLLECTION_FIXTURE", temporary.path())
        .env("ZUP_TEST_FAULT_POINT", point).env("ZUP_TEST_FAULT_ACTION", "crash")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
    assert!(!result.success());
    let installation = native_publisher(&state).load(&second.app_id, SelectedScope::User).unwrap().unwrap();
    let store = FilesystemTransactionStore::private_at(&state);
    use zup_transaction::TransactionStore;
    let record = store.load(&installation.commit.transaction).unwrap();
    assert_eq!(record.phase, zup_transaction::TransactionPhase::Committed);
    assert!(!record.cleanup_complete());
    let mut executor = native_executor(&state, &source, &record);
    let (record, outcome) = zup_transaction::recover(record, &store, &mut executor, &native_publisher(&state)).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed, "{:?}", record.recovery_failure);
    assert!(record.cleanup_complete());
    assert!(!Path::new(first.current.materialization.as_ref().unwrap().directory().as_str()).exists());
    let installation = native_publisher(&state).load(&second.app_id, SelectedScope::User).unwrap().unwrap();
    assert_eq!(installation.current, second.current);
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn retired_fixed_collection_removes_only_the_recorded_backup(#[case] changed_backup: bool) {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    let key = zup_core::ResourceKey::File { id: zup_core::FileResourceId::Payload { id: zup_core::PayloadId::File { declaration: zup_core::ResourceId::new("fixed").unwrap() } } };
    #[cfg(windows)]
    std::fs::copy(r"C:\Windows\System32\PING.EXE", source.join("build/runner.bin")).unwrap();
    #[cfg(not(windows))]
    std::fs::copy("/usr/bin/sleep", source.join("build/runner.bin")).unwrap();
    let mut previous: Option<InstallationRecord> = None;
    for contents in [b"first".as_slice(), b"second".as_slice()] {
        std::fs::write(source.join("build/asset.bin"), contents).unwrap();
        std::fs::write(source.join("build/fixed.bin"), contents).unwrap();
        let mut effects = native_effects(&application, &source, contents, previous.as_ref());
        let destination = effects.install_directory.as_ref().unwrap().join("fixed.bin").unwrap();
        let old = previous.as_ref().and_then(|record| record.current.resources.get(&key));
        let precondition = match old {
            Some(zup_plan::OwnedResource::File { instance, size, sha256, .. }) => zup_plan::FilePrecondition::Exact { object: instance.object, size: *size, sha256: *sha256 },
            None => zup_plan::FilePrecondition::Absent,
            _ => panic!("fixed fixture ownership differs"),
        };
        effects.files.push(zup_plan::FileOperation {
            key: key.clone(), replacement: zup_core::FixedReplacement::Quiescent, source_relative: RelativePath::new("build/fixed.bin").unwrap(),
            destination: destination.clone(), precondition, expected_sha256: hash_bytes(contents), expected_size: contents.len() as u64, executable: false, privilege: Privilege::User,
            kind: if old.is_some() { zup_plan::FileOperationKind::Replace } else { zup_plan::FileOperationKind::Create }, conflict: None,
        });
        effects.requirements.files.push(zup_plan::RequiredFile { key: key.clone(), placement: zup_core::PayloadPlacement::Fixed { path: zup_core::Template::parse(destination.as_str()).unwrap(), replacement: zup_core::FixedReplacement::Quiescent }, size: contents.len() as u64, sha256: hash_bytes(contents), executable: false, privilege: Privilege::User });
        effects.requirements.files.sort_by(|a, b| a.key.cmp(&b.key));
        previous = Some(commit_effects(&state, &source, effects));
    }
    let installation = previous.unwrap();
    let retirement = zup_plan::RetentionChange::keeping_previous(&installation, 0);
    let candidates = zup_transaction::cleanup_candidates(&FilesystemTransactionStore::private_at(&state), &installation, retirement.as_ref()).unwrap();
    let backup = candidates.iter().find_map(|operation| match &operation.candidate { zup_plan::CleanupCandidate::Fixed(file) => Some(file.path.clone()), _ => None }).unwrap();
    assert_eq!(std::fs::read(backup.as_str()).unwrap(), b"first");
    if changed_backup { std::fs::write(backup.as_str(), b"foreign content").unwrap(); }
    let (record, outcome) = maintain(&state, &source, &installation, retirement);
    if changed_backup {
        assert_eq!(outcome, TransactionOutcome::RecoveryRequired);
        assert_eq!(record.phase, zup_transaction::TransactionPhase::Committed);
        assert_eq!(std::fs::read(backup.as_str()).unwrap(), b"foreign content");
    } else {
        assert_eq!(outcome, TransactionOutcome::Committed, "{:?}", record.recovery_failure);
        assert!(!Path::new(backup.as_str()).exists());
        let current = native_publisher(&state).load(&installation.app_id, SelectedScope::User).unwrap().unwrap();
        assert!(zup_transaction::cleanup_candidates(&FilesystemTransactionStore::private_at(&state), &current, None).unwrap().is_empty());
    }
    if !changed_backup {
        let collected = native_publisher(&state).load(&installation.app_id, SelectedScope::User).unwrap().unwrap();
        assert!(!collected.retired_deployments.is_empty());
        let (_, outcome) = maintain(&state, &source, &collected, None);
        assert_eq!(outcome, TransactionOutcome::Committed);
        let pruned = native_publisher(&state).load(&installation.app_id, SelectedScope::User).unwrap().unwrap();
        assert!(pruned.retired_deployments.is_empty());
        assert_eq!(pruned.current, installation.current);
    }
    assert_eq!(std::fs::read(application.join("fixed.bin")).unwrap(), b"second");
}

#[test]
fn default_reader_contract_retains_retired_generations() {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"first").unwrap();
    let first = deploy(&state, &application, &source, b"first", None);
    std::fs::write(source.join("build/asset.bin"), b"second").unwrap();
    let second = deploy(&state, &application, &source, b"second", Some(&first));
    let (record, outcome) = maintain(&state, &source, &second, zup_plan::RetentionChange::keeping_previous(&second, 0));
    assert_eq!(outcome, TransactionOutcome::Committed, "{:?}", record.recovery_failure);
    for node in &record.plan.nodes {
        if let zup_plan::NodeKind::Cleanup { operation } = &node.kind
            && let zup_plan::CleanupCandidate::Generation(generation) = &operation.candidate
            && generation.materialization == *first.current.materialization.as_ref().unwrap()
        {
            assert!(matches!(record.receipt(&node.id), Some(zup_transaction::OperationReceipt::Cleanup { completion: zup_transaction::CleanupCompletion::Deferred { reason: zup_transaction::CleanupDeferral::UntrackedReaders }, .. })));
        }
    }
    assert!(Path::new(first.current.materialization.as_ref().unwrap().directory().as_str()).join("lib/data.txt").exists());
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn compensated_generation_is_collected_without_retiring_the_active_snapshot(#[case] already_absent: bool) {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    #[cfg(windows)]
    std::fs::copy(r"C:\Windows\System32\PING.EXE", source.join("build/runner.bin")).unwrap();
    #[cfg(not(windows))]
    std::fs::copy("/usr/bin/sleep", source.join("build/runner.bin")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"first").unwrap();
    let first = commit_effects(&state, &source, native_effects(&application, &source, b"first", None));
    std::fs::write(source.join("build/asset.bin"), b"second").unwrap();
    let effects = native_effects(&application, &source, b"second", Some(&first));
    let failed_path = effects.generations[0].materialization.directory().clone();
    let plan = compile_deployment(&effects).unwrap();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state))
        .begin(first.app_id.clone(), first.scope, first.current.version.clone(), plan).unwrap();
    let mut executor = super::persistent_lifecycle::FailActivation(native_executor(&state, &source, open.record()));
    let publisher = native_publisher(&state);
    let settled = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RolledBack);
    assert!(Path::new(failed_path.as_str()).exists());
    let installation = publisher.load(&first.app_id, first.scope).unwrap().unwrap();
    assert_eq!(installation.current, first.current);
    if already_absent {
        let root = Path::new(failed_path.as_str());
        std::fs::remove_file(root.join(if cfg!(windows) { "bin/runner.exe" } else { "bin/sleep" })).unwrap();
        std::fs::remove_file(root.join("lib/data.txt")).unwrap();
        std::fs::remove_dir(root.join("bin")).unwrap();
        std::fs::remove_dir(root.join("lib")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    let (collected, outcome) = maintain(&state, &source, &installation, None);
    assert_eq!(outcome, TransactionOutcome::Committed);
    assert!(collected.plan.nodes.iter().any(|node| matches!(collected.receipt(&node.id),
        Some(zup_transaction::OperationReceipt::Cleanup { completion, .. })
            if *completion == if already_absent { zup_transaction::CleanupCompletion::AlreadyAbsent } else { zup_transaction::CleanupCompletion::Removed })));
    assert!(!Path::new(failed_path.as_str()).exists());
    let installation = publisher.load(&first.app_id, first.scope).unwrap().unwrap();
    assert_eq!(installation.current, first.current);
    assert!(settled.record.nodes.values().any(|state| matches!(state,
        zup_transaction::NodeState::Compensated { receipt }
            if matches!(receipt.as_ref(), zup_transaction::OperationReceipt::Generation(receipt)
                if matches!(receipt.as_ref(), zup_transaction::GenerationReceipt::Sealed { .. })))));
}
