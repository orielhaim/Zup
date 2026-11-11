#[test]
fn uninstall_preserves_mutable_data_and_reattaches_the_same_owned_root() {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"generation-a").unwrap();
    let installed = deploy(&state, &application, &source, b"generation-a", None);
    let owned = installed.data.values().next().unwrap();
    let database = Path::new(owned.directory.location.path().unwrap().as_str()).join("database.sqlite");
    std::fs::write(&database, b"data written after installation").unwrap();

    let mut effects = DeploymentEffects::new(installed.id, installed.current.requirements.clone());
    effects.transition = zup_plan::DeploymentTransition::Remove;
    effects.observed = Some(installed.current.selection());
    effects.observed_transaction = Some(installed.current.transaction);
    effects.install_directory = Some(installed.directory.clone());
    effects.bind_persistent(None, Some(&installed), None, &Default::default()).unwrap();
    let removed = commit_effects(&state, &source, effects);
    assert!(!removed.is_installed());
    assert_eq!(removed.data, installed.data);
    assert_eq!(std::fs::read(&database).unwrap(), b"data written after installation");

    let reinstalled = deploy(&state, &application, &source, b"generation-a", Some(&removed));
    assert!(reinstalled.is_installed());
    assert_eq!(reinstalled.data, installed.data);
    assert_eq!(std::fs::read(&database).unwrap(), b"data written after installation");
}

pub(super) struct FailActivation<E>(pub(super) E);
impl<E: zup_transaction::OperationExecutor> zup_transaction::OperationExecutor for FailActivation<E>
where E::Error: std::fmt::Display {
    type Error = String;
    fn prepare(&mut self, node: &zup_plan::DeploymentNode) -> Result<(), String> { self.0.prepare(node).map_err(|error| error.to_string()) }
    fn apply(&mut self, node: &zup_plan::DeploymentNode, record: &zup_transaction::TransactionRecord) -> Result<zup_transaction::OperationReceipt, String> {
        let receipt = self.0.apply(node, record).map_err(|error| error.to_string())?;
        if let zup_transaction::OperationReceipt::PersistentDirectory(owned) = &receipt {
            std::fs::write(Path::new(owned.owned.directory.location.path().unwrap().as_str()).join("database.sqlite"), b"preserved despite failed activation").map_err(|error| error.to_string())?;
        }
        Ok(receipt)
    }
    fn verify(&mut self, node: &zup_plan::DeploymentNode, receipt: &zup_transaction::OperationReceipt) -> Result<(), String> {
        if matches!(node.kind, zup_plan::NodeKind::Generation { operation: zup_plan::GenerationOperation::Activate, .. }) {
            return Err("injected activation verification failure".into());
        }
        self.0.verify(node, receipt).map_err(|error| error.to_string())
    }
    fn rollback(&mut self, node: &zup_plan::DeploymentNode, receipt: &zup_transaction::OperationReceipt) -> Result<(), String> { self.0.rollback(node, receipt).map_err(|error| error.to_string()) }
    fn reconcile(&mut self, node: &zup_plan::DeploymentNode, receipt: Option<&zup_transaction::OperationReceipt>, record: &zup_transaction::TransactionRecord) -> Result<zup_transaction::ReconcileResult, String> { self.0.reconcile(node, receipt, record).map_err(|error| error.to_string()) }
    fn reconcile_rollback(&mut self, node: &zup_plan::DeploymentNode, receipt: &zup_transaction::OperationReceipt) -> Result<zup_transaction::RollbackReconcileResult, String> { self.0.reconcile_rollback(node, receipt).map_err(|error| error.to_string()) }
}

#[test]
fn failed_initial_install_reattaches_only_its_authenticated_data_claim() {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"generation-a").unwrap();
    let effects = deployment_effects(&application, &source, b"generation-a", None, None);
    let plan = compile_deployment(&effects).unwrap();
    let target = plan.target;
    let app = plan.requirements.app.clone();
    let open = TransactionCoordinator::new(FilesystemTransactionStore::private_at(&state)).begin(app.id.clone(), SelectedScope::User, app.version, plan).unwrap();
    let mut executor = FailActivation(native_executor(&state, &source, open.record()));
    let publisher = native_publisher(&state);
    let settled = open.execute(&mut executor, &publisher).unwrap();
    assert_eq!(settled.outcome, TransactionOutcome::RolledBack);
    assert!(publisher.load(&app.id, SelectedScope::User).unwrap().is_none());
    let claims = publisher.persistent_claims(&app.id, SelectedScope::User, target).unwrap().unwrap();
    assert_eq!(claims.installation, settled.record.plan.installation);
    let owned = claims.owners.values().next().unwrap();
    let database = Path::new(owned.directory.location.path().unwrap().as_str()).join("database.sqlite");
    assert_eq!(std::fs::read(&database).unwrap(), b"preserved despite failed activation");
    let retry = deployment_effects(&application, &source, b"generation-a", None, Some(&claims));
    let mut forged = retry.clone();
    let forged_origin = zup_core::TransactionId::new_v7();
    for operation in &mut forged.data { operation.intent.previous.as_mut().unwrap().origin = forged_origin; }
    assert!(publisher.validate_plan(&app.id, SelectedScope::User, &compile_deployment(&forged).unwrap()).is_err());
    let installed = commit_effects(&state, &source, retry);
    assert_eq!(installed.data.get(&owned.directory.id), Some(owned));
    assert!(installed.references_transaction(settled.record.transaction_id));
    assert_eq!(std::fs::read(&database).unwrap(), b"preserved despite failed activation");
}

#[test]
fn retained_activation_reuses_verified_content_without_the_original_payload() {
    #[cfg(windows)]
    let temporary = tempfile::tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir()).unwrap();
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let application = temporary.path().join("application");
    let source = temporary.path().join("source");
    std::fs::create_dir_all(source.join("build")).unwrap();
    std::fs::write(source.join("build/asset.bin"), b"generation-a").unwrap();
    let a = deploy(&state, &application, &source, b"generation-a", None);
    let owned = a.data.values().next().unwrap();
    let database = Path::new(owned.directory.location.path().unwrap().as_str()).join("database.sqlite");
    std::fs::write(&database, b"persistent contents").unwrap();
    std::fs::write(source.join("build/asset.bin"), b"generation-b").unwrap();
    let b = deploy(&state, &application, &source, b"generation-b", Some(&a));
    std::fs::remove_file(source.join("build/asset.bin")).unwrap();
    let retained = &a.current;
    let desired = b.retained_deployment(retained.revision).unwrap();
    desired.validate().unwrap();
    #[cfg(windows)]
    let target = zup_windows::resolve_target(&desired, &zup_windows::WindowsTargetContext::new(SelectedScope::User)).unwrap();
    #[cfg(not(windows))]
    let target = zup_linux::resolve_target(&desired, &zup_linux::LinuxInstallLocationResolver::default()).unwrap();
    assert_eq!(target.requirements, retained.requirements);
    let mut effects = DeploymentEffects::new(b.id, target.requirements.clone());
    effects.observed = Some(b.current.selection());
    effects.observed_transaction = Some(b.current.transaction);
    effects.install_directory = Some(b.directory.clone());
    effects.bind_persistent(Some(&target), Some(&b), None, &target.data.iter().map(|data| (data.id.clone(), zup_plan::PersistentBinding::Filesystem(zup_plan::FilesystemDataBinding { path: data.location.path().unwrap().clone(), writers: Vec::new() }))).collect()).unwrap();
    effects.generations.push(target.generation.as_ref().unwrap().bind(Privilege::User, Default::default()).unwrap());
    let reactivated = commit_effects(&state, &source, effects);
    assert_eq!(reactivated.current.materialization, retained.materialization);
    assert!(reactivated.current.revision > b.current.revision);
    assert_eq!(std::fs::read(Path::new(reactivated.current.materialization.as_ref().unwrap().directory().as_str()).join("lib/data.txt")).unwrap(), b"generation-a");
    assert_eq!(reactivated.data, b.data);
    assert_eq!(std::fs::read(database).unwrap(), b"persistent contents");
}
