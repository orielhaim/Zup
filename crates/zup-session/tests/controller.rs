use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use zup_core::{SelectedScope, Target};
use zup_session::{OperationContext, SessionDiscovery, SessionHost, SessionLaunch};

#[derive(Clone, Default)]
struct Hold {
    started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum Script {
    Recovery(zup_core::TransactionId),
    Commit,
    Panic,
    CommitAfterCancel,
}

#[derive(Clone)]
struct FakeBackend {
    ran: Arc<AtomicBool>,
    hold: Option<Hold>,
    script: Script,
}

impl zup_runtime::RuntimeBackend for FakeBackend {
    fn execute<'a>(
        &'a self,
        _request: zup_runtime::RuntimeRequest,
        control: zup_runtime::RuntimeControl,
    ) -> zup_runtime::RuntimeFuture<
        'a,
        Result<zup_runtime::InstallOutcome, zup_runtime::SessionError>,
    > {
        Box::pin(async move {
            if let Some(hold) = &self.hold {
                let _ = control.events.send(zup_runtime::RuntimeEvent::Progress {
                    phase: zup_core::OperationPhase::Files,
                    completed: 0,
                    total: 1,
                    action: "holding".into(),
                });
                hold.started.store(true, Ordering::SeqCst);
                while !control.cancellation.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                hold.cancelled.store(true, Ordering::SeqCst);
                while !hold.release.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            self.ran.store(true, Ordering::SeqCst);
            match self.script {
                Script::Recovery(transaction) => {
                    Ok(zup_runtime::InstallOutcome::RecoveryRequired {
                        transaction: Some(transaction.0),
                        detail: "activation outcome is uncertain".into(),
                    })
                }
                Script::Panic => panic!("operation task failed"),
                Script::CommitAfterCancel => {
                    let _ = control.events.send(zup_runtime::RuntimeEvent::Progress {
                        phase: zup_core::OperationPhase::Files,
                        completed: 0,
                        total: 1,
                        action: "holding".into(),
                    });
                    while !control.cancellation.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(zup_runtime::InstallOutcome::Committed)
                }
                Script::Commit => Ok(zup_runtime::InstallOutcome::Committed),
            }
        })
    }
}

#[derive(Clone, Default)]
struct Seen {
    events: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct FakeHost {
    build: zup_core::TargetBuildPlan,
    backend: FakeBackend,
    preset: PathBuf,
    installations: Vec<zup_session::ScopedInstallation>,
    seen: Seen,
    refuse_backend: bool,
    after_commit_error: Option<String>,
    journal: Option<Arc<JournalRecovery>>,
    ledger_error: Arc<AtomicBool>,
    refresh_failures: Arc<std::sync::atomic::AtomicUsize>,
}

fn fixture_build() -> zup_core::TargetBuildPlan {
    zup_core::TargetBuildPlan {
        installer: zup_core::Installer {
            preset: None,
            app: zup_core::App {
                id: zup_core::AppId::new("com.example.session").expect("valid id"),
                name: zup_core::NonEmptyString::new("Session").expect("valid name"),
                version: semver::Version::new(1, 0, 0),
                publisher: None,
                main: None,
                description: None,
            },
            target: Target::parse("windows-x64").expect("valid target"),
            frontend: zup_core::Frontend::Gui,
            updates: None,
            install: zup_core::Install {
                generation_readers: Default::default(),
                data: Vec::new(),
                scope: zup_core::InstallScope::User,
                directory: zup_core::InstallDirectory {
                    user: None,
                    machine: None,
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            component_groups: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        files: Vec::new(),
        ui_assets: Vec::new(),
        total_size: 0,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
    }
}

fn fixture_target() -> zup_plan::BoundDeployment {
    let target = Target::parse("windows-x64").expect("valid target");
    zup_plan::DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: "00000000-0000-7000-8000-000000000001".parse().unwrap(),
        generation: None,
        app: zup_core::App {
            id: zup_core::AppId::new("com.example.session").expect("valid id"),
            name: zup_core::NonEmptyString::new("Session").expect("valid name"),
            version: semver::Version::new(1, 0, 0),
            publisher: None,
            main: None,
            description: None,
        },
        target,
        scope: SelectedScope::User,
        install_directory: zup_core::TargetPath::new(target, "C:\\Session").expect("valid path"),
        selected_components: Vec::new(),
        prerequisites: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),

        preset: None,
    }
    .bind_fixed()
    .expect("fixed deployment requirements")
}

fn fixture_install() -> zup_plan::DesiredState {
    zup_plan::DesiredState {
        generation_readers: Default::default(),
        data: Vec::new(),
        requirements: (),
        maintenance: None,
        installation: fixture_target().installation,
        generation: None,
        app: zup_core::App {
            id: zup_core::AppId::new("com.example.session").expect("valid id"),
            name: zup_core::NonEmptyString::new("Session").expect("valid name"),
            version: semver::Version::new(1, 0, 0),
            publisher: None,
            main: None,
            description: None,
        },
        target: Target::parse("windows-x64").expect("valid target"),
        scope: SelectedScope::User,
        install_directory: zup_core::Template::parse("${install}").expect("valid template"),
        selected_components: Vec::new(),
        prerequisites: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),

        preset: None,
    }
}

fn installed(scope: SelectedScope) -> zup_plan::InstallationRecord {
    let desired = fixture_target();
    {
        let fixture_commit = zup_plan::InstallationCommit {
            transaction: zup_core::TransactionId::new_v7(),
            plan_hash: zup_core::hash_bytes(b"session installation"),
        };
        zup_plan::InstallationRecord {
            commit: fixture_commit,
            retired_deployments: Vec::new(),
            data: Default::default(),
            id: desired.installation,
            state: zup_plan::InstallationState::Installed,
            app_id: desired.app.id,
            scope,
            target: desired.target,
            directory: desired.install_directory,
            current: zup_plan::DeploymentSnapshot {
                maintenance: None,
                requirements: zup_plan::DeploymentRequirements {
                    scope,
                    ..desired.requirements.clone()
                },
                revision: zup_core::ActivationRevision::FIRST,
                transaction: fixture_commit.transaction,
                plan_hash: fixture_commit.plan_hash,
                version: desired.app.version,
                selected_components: Vec::new(),
                release: None,
                preset: None,
                materialization: None,
                resources: BTreeMap::new(),
            },
            retained: Vec::new(),
        }
    }
}

impl zup_runtime::OperationHost for FakeHost {
    fn plan_build(
        &self,
        _build: &zup_core::TargetBuildPlan,
        request: &zup_plan::PlanRequest,
        _cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PlannedInstallation, zup_runtime::OperationError> {
        let mut plan = fixture_install();
        plan.installation = request.installation;
        plan.scope = request.scope;
        plan.selected_components =
            zup_plan::select_components(&_build.installer.components, &request.components).unwrap();
        Ok(zup_plan::PlannedInstallation {
            plan,
            generated_files: Vec::new(),
        })
    }

    fn resolve_target(
        &self,
        install: &zup_plan::DesiredState,
    ) -> Result<zup_plan::BoundDeployment, String> {
        let mut target = fixture_target();
        target.installation = install.installation;
        target.scope = install.scope;
        target.selected_components = install.selected_components.clone();
        target.requirements.components = install.selected_components.clone();
        target.requirements.scope = install.scope;
        Ok(target)
    }

    fn observe(
        &self,
        _target: Option<&zup_plan::BoundDeployment>,
        _ledger: Option<&zup_plan::InstallationRecord>,
    ) -> Result<
        (
            Option<zup_plan::HostSnapshot>,
            BTreeMap<zup_core::ResourceKey, bool>,
        ),
        String,
    > {
        Ok((Some(zup_plan::HostSnapshot::default()), BTreeMap::new()))
    }

    fn compile(
        &self,
        execution: &zup_plan::ExecutionPlan,
        _target: Option<&zup_plan::BoundDeployment>,
        _ledger: Option<&zup_plan::InstallationRecord>,
    ) -> Result<zup_plan::DeploymentEffects, String> {
        let binding = execution.binding.as_ref().expect("bound execution");
        let mut effects =
            zup_plan::DeploymentEffects::new(binding.installation, binding.requirements.clone());
        effects.selected_components = execution.selected_components.clone();
        effects.install_directory = execution.install_directory.clone();
        effects.observed = binding.observed_activation;
        effects.observed_transaction = binding.observed_transaction;
        effects.transition = execution.transition;
        Ok(effects)
    }

    fn attach_content(
        &self,
        _target: &mut zup_plan::BoundDeployment,
        _content: &zup_runtime::ContentAccess,
        _preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        Ok(())
    }
}

impl SessionHost for FakeHost {
    type Bound<'a>
        = Self
    where
        Self: 'a;

    fn state_root(
        &self,
        _installer: &zup_core::Installer,
        scope: SelectedScope,
        hint: Option<&Path>,
    ) -> Result<PathBuf, String> {
        Ok(hint
            .map(Path::to_owned)
            .or_else(|| {
                self.installations
                    .iter()
                    .find(|i| i.scope == scope)
                    .map(|i| i.state_root.clone())
            })
            .unwrap_or_default())
    }

    fn pending_recovery(
        &self,
        context: &OperationContext,
        app: &zup_core::AppId,
    ) -> Result<Option<zup_core::RecoverySummary>, String> {
        if let Script::Recovery(transaction) = self.backend.script
            && !self
                .seen
                .events
                .lock()
                .unwrap()
                .contains(&format!("recover {transaction}"))
        {
            return Ok(Some(zup_core::RecoverySummary {
                transaction,
                installation: zup_core::InstallationId::new_v7(),
                app: app.clone(),
                version: self.build.installer.app.version.clone(),
                scope: context.scope,
                target: self.build.installer.target,
                directory: None,
                kind: zup_core::RecoveryKind::Interrupted,
                cleanup: Vec::new(),
                detail: "activation outcome is uncertain".into(),
            }));
        }
        if self.journal.is_none() {
            return Ok(None);
        }
        self.recovery_store(context, app)?
            .pending_recovery(app, context.scope)
            .map(|target| target.map(|target| target.summary()))
            .map_err(|error| error.to_string())
    }

    fn bind<'a>(&'a self, context: &'a OperationContext) -> Result<Self, String> {
        self.seen.events.lock().expect("seen").push(format!(
            "bind {} {}",
            context.scope,
            context.state_root.display()
        ));
        Ok(self.clone())
    }

    fn describe(&mut self, _executable: &Path) -> Result<Arc<zup_core::TargetBuildPlan>, String> {
        Ok(Arc::new(self.build.clone()))
    }

    fn recover<'a>(
        &'a self,
        _context: &'a OperationContext,
        target: zup_core::RecoverySummary,
        request: zup_transaction::RecoveryRequest,
        _control: &'a zup_runtime::OperationControl,
    ) -> zup_runtime::RuntimeFuture<
        'a,
        Result<zup_runtime::InstallOutcome, zup_runtime::SessionError>,
    > {
        let transaction = target.transaction;
        Box::pin(async move {
            if let Some(journal) = &self.journal {
                use zup_transaction::TransactionStore;
                let store =
                    zup_transaction::FilesystemTransactionStore::private_at(&_context.state_root);
                let record = store.load(&transaction).unwrap();
                assert_eq!(record.app_id, self.build.installer.app.id);
                assert_eq!(record.scope, _context.scope);
                assert_eq!(record.plan.installation, target.installation);
                assert_eq!(record.target, target.target);
                let mut executor = journal.executor.lock().unwrap();
                let attempt = journal.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt > 0 {
                    executor.fail_verify_operation = None;
                }
                let (record, outcome) = zup_transaction::recover_requested(
                    record,
                    &store,
                    &mut *executor,
                    &journal.publisher,
                    &request,
                )
                .unwrap();
                self.seen
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("recover {transaction}"));
                return Ok(match outcome {
                    zup_transaction::TransactionOutcome::Committed => {
                        zup_transaction::reconcile_committed(
                            &store,
                            &record,
                            &_context.state_root,
                            |_| Ok(()),
                        )
                        .unwrap();
                        self.ledger_error.store(false, Ordering::SeqCst);
                        zup_runtime::InstallOutcome::Recovered {
                            settlement: zup_runtime::RecoverySettlement::Committed,
                        }
                    }
                    zup_transaction::TransactionOutcome::RecoveryRequired => {
                        zup_runtime::InstallOutcome::recovery(&record)
                    }
                    other => panic!("committed cleanup did not remain committed: {other:?}"),
                });
            }
            assert_eq!(request, zup_transaction::RecoveryRequest::Retry {});
            assert!(
                matches!(self.backend.script, Script::Recovery(expected) if expected == transaction)
            );
            self.seen
                .events
                .lock()
                .unwrap()
                .push(format!("recover {transaction}"));
            Ok(zup_runtime::InstallOutcome::Recovered {
                settlement: zup_runtime::RecoverySettlement::RolledBack,
            })
        })
    }

    fn refresh_installation(
        &mut self,
        _context: &OperationContext,
    ) -> Result<Option<Arc<zup_core::TargetBuildPlan>>, String> {
        self.seen.events.lock().unwrap().push("refreshed".into());
        if self
            .refresh_failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            return Err("presentation refresh unavailable".into());
        }
        Ok(None)
    }

    fn observe(
        &self,
        _installer: &zup_core::Installer,
        scope: SelectedScope,
        state_root: Option<&Path>,
    ) -> Result<OperationContext, String> {
        if self.ledger_error.load(Ordering::SeqCst) {
            return Err("installation ledger could not be read".into());
        }
        let installation = self
            .installations
            .iter()
            .find(|installation| installation.scope == scope);
        Ok(OperationContext {
            scope,
            state_root: state_root
                .map(Path::to_path_buf)
                .or_else(|| installation.map(|installation| installation.state_root.clone()))
                .unwrap_or_default(),
            ledger: installation.and_then(|installation| installation.ledger.clone()),
        })
    }

    fn backend(
        &self,
        context: &OperationContext,
        prepared: &zup_runtime::PreparedOperation,
    ) -> Result<Arc<dyn zup_runtime::RuntimeBackend>, String> {
        self.seen.events.lock().expect("seen").push(format!(
            "backend {} {} uninstall={} plan={}",
            context.scope,
            context.state_root.display(),
            prepared.action == zup_plan::LifecycleAction::Uninstall,
            prepared.install_plan().is_some()
        ));
        if self.refuse_backend {
            return Err("backend initialization failed".into());
        }
        Ok(Arc::new(self.backend.clone()))
    }

    fn after_commit(&self, _context: &OperationContext) -> Result<(), String> {
        match &self.after_commit_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
        _context: &OperationContext,
    ) -> Result<zup_session::PresetLaunch, String> {
        Ok(zup_session::PresetLaunch {
            executable: self.preset.clone(),
            configuration: zup_preset_protocol::Configuration {
                settings: serde_json::json!({}),
                assets: BTreeMap::new(),
            },
            capabilities: zup_preset_host::surface::capabilities(discovery.installer(), false),
            product: zup_preset_host::surface::product(discovery.installer()),
            environment: zup_preset_protocol::environment::current(
                discovery.installer().target.as_str(),
            ),
            stage: None,
        })
    }
}

fn host(preset: &str) -> FakeHost {
    let mut stub = PathBuf::from(env!("CARGO_BIN_EXE_close_preset"));
    stub.set_file_name(format!("{preset}{}", std::env::consts::EXE_SUFFIX));
    FakeHost {
        build: fixture_build(),
        backend: FakeBackend {
            ran: Arc::new(AtomicBool::new(false)),
            hold: None,
            script: Script::Commit,
        },
        preset: stub,
        installations: vec![zup_session::ScopedInstallation {
            scope: SelectedScope::User,
            state_root: tempfile::tempdir().unwrap().keep(),
            ledger: None,
        }],
        seen: Seen::default(),
        refuse_backend: false,
        after_commit_error: None,
        journal: None,
        ledger_error: Default::default(),
        refresh_failures: Default::default(),
    }
}

fn fake_exe() -> &'static Path {
    Path::new("session-test")
}

#[tokio::test]
async fn closing_during_an_operation_waits_for_settlement() {
    let hold = Hold::default();
    let mut session_host = host("install_close_preset");
    session_host.backend.hold = Some(hold.clone());
    let cancelled = Arc::clone(&hold.cancelled);
    let release = Arc::clone(&hold.release);
    let session = tokio::spawn(zup_session::run(
        session_host.clone(),
        fake_exe(),
        SessionLaunch::default(),
    ));
    let wait = tokio::time::timeout(Duration::from_secs(10), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    wait.await.expect("close reaches the running operation");
    assert!(
        !session.is_finished(),
        "closing the preset must not finish the session while the operation is held"
    );
    release.store(true, Ordering::SeqCst);
    session
        .await
        .expect("session task")
        .expect("the session settles");
    assert!(session_host.backend.ran.load(Ordering::SeqCst));
}

#[tokio::test]
async fn backend_initialization_failure_settles_the_operation() {
    let mut session_host = host("install_preset");
    session_host.refuse_backend = true;
    zup_session::run(session_host, fake_exe(), SessionLaunch::default())
        .await
        .expect("a failed operation still settles the session");
}

#[tokio::test]
async fn post_commit_cleanup_failure_is_one_reconciliation_result() {
    let root = tempfile::tempdir().expect("state root");
    let mut session_host = host("close_preset");
    session_host.after_commit_error = Some("cleanup failed".into());
    let ledger = installed(SelectedScope::User);
    session_host.installations = vec![zup_session::ScopedInstallation {
        scope: SelectedScope::User,
        state_root: root.path().to_path_buf(),
        ledger: Some(ledger),
    }];
    let discovery = session_host
        .discover(fake_exe(), &SessionLaunch::default())
        .expect("discovery");
    let control = zup_runtime::OperationControl::new(zup_runtime::ExecutionPolicy::Interactive);
    let mut received = control.subscribe();
    let selection = zup_session::Selection {
        scope: SelectedScope::User,
        components: Vec::new(),
        install_directory: None,
    };
    let completion = zup_session::complete_operation(
        &session_host,
        fake_exe(),
        &discovery,
        zup_session::OperationLaunch {
            id: uuid::Uuid::now_v7(),
            action: zup_plan::LifecycleAction::Uninstall,
            selection: &selection,
            control,
        },
    )
    .await;
    assert!(
        matches!(
            completion.outcome,
            zup_session::SessionOutcome::Deployment(
                zup_runtime::InstallOutcome::CommittedWithWarning { .. }
            )
        ),
        "cleanup after commit stays a committed reconciliation: {:?}",
        completion.outcome
    );
    let zup_session::SessionOutcome::Deployment(ref outcome) = completion.outcome else {
        panic!("deployment result expected");
    };
    let context = discovery.context(SelectedScope::User).unwrap();
    let mut state = session_host.open_state(&discovery, &context, None);
    state.finish_with(outcome);
    assert_ne!(
        state.snapshot().state,
        zup_preset_protocol::InstallerState::RecoveryRequired,
        "nonjournal cleanup must not invent journal recovery"
    );
    assert!(
        !matches!(
            state.accept(zup_preset_protocol::Action::Retry),
            zup_session::HostDecision::Refused(_)
        ),
        "cleanup diagnostic must have a safe action"
    );
    assert!(
        std::iter::from_fn(|| received.try_recv().ok())
            .all(|event| !zup_session::is_terminal(&event)),
        "the operation result is not also a progress event"
    );
}

#[tokio::test]
async fn a_preset_close_ends_the_session() {
    let host = host("close_preset");
    zup_session::run(host, fake_exe(), SessionLaunch::default())
        .await
        .expect("a close ends the session");
}

#[tokio::test]
async fn an_install_runs_the_shared_operation_to_commit() {
    let host = host("install_preset");
    zup_session::run(host.clone(), fake_exe(), SessionLaunch::default())
        .await
        .expect("an install runs to completion");
    assert!(
        host.backend.ran.load(Ordering::SeqCst),
        "the preset action reached the backend through the shared operation"
    );
}

#[tokio::test]
async fn a_preset_recovers_the_reported_transaction_and_refreshes_state() {
    let mut host = host("recovery_preset");
    let transaction = zup_core::TransactionId::new_v7();
    host.backend.script = Script::Recovery(transaction);
    zup_session::run(host.clone(), fake_exe(), SessionLaunch::default())
        .await
        .expect("recovery settles");
    let seen = host.seen.events.lock().unwrap();
    assert!(seen.contains(&format!("recover {transaction}")));
    assert_eq!(
        seen.iter()
            .filter(|event| event.as_str() == "refreshed")
            .count(),
        1
    );
}

#[test]
fn an_unspecified_either_scope_presents_an_existing_installation() {
    let scope = zup_session::select_scope(
        zup_core::InstallScope::Either,
        &SessionLaunch {
            interactive: true,
            ..SessionLaunch::default()
        },
        &[zup_session::ScopedInstallation {
            scope: SelectedScope::Machine,
            state_root: PathBuf::from("machine"),
            ledger: Some(installed(SelectedScope::Machine)),
        }],
        &Default::default(),
    )
    .expect("presentation");
    assert_eq!(scope, SelectedScope::Machine);
    let headless = zup_session::select_scope(
        zup_core::InstallScope::Either,
        &SessionLaunch::default(),
        &[],
        &Default::default(),
    );
    assert!(
        headless.is_err(),
        "a console either install names its scope"
    );
}

#[tokio::test]
async fn switching_scope_plans_and_executes_against_that_installation() {
    let user_root = tempfile::tempdir().expect("user root");
    let machine_root = tempfile::tempdir().expect("machine root");
    let mut host = host("scope_install_preset");
    host.build.installer.install.scope = zup_core::InstallScope::Either;
    host.installations = vec![
        zup_session::ScopedInstallation {
            scope: SelectedScope::User,
            state_root: user_root.path().to_path_buf(),
            ledger: None,
        },
        zup_session::ScopedInstallation {
            scope: SelectedScope::Machine,
            state_root: machine_root.path().to_path_buf(),
            ledger: None,
        },
    ];
    let seen = host.seen.clone();
    zup_session::run(
        host,
        fake_exe(),
        SessionLaunch {
            interactive: true,
            ..SessionLaunch::default()
        },
    )
    .await
    .expect("scope switch installs");
    let events = seen.events.lock().expect("seen");
    let backend = events
        .iter()
        .rev()
        .find(|event| event.starts_with("backend "))
        .expect("the install reached a backend");
    assert!(
        backend.contains(&machine_root.path().display().to_string()),
        "execution uses the machine installation: {backend}"
    );
    assert!(
        backend.contains("plan=true"),
        "an install carries its plan: {backend}"
    );
    assert!(
        events.iter().any(|event| event.starts_with("bind user")),
        "the initial preview is bound to the presented user scope"
    );
}

#[tokio::test]
async fn switching_from_machine_to_user_uses_the_user_installation() {
    let user_root = tempfile::tempdir().expect("user root");
    let machine_root = tempfile::tempdir().expect("machine root");
    let mut host = host("user_scope_install_preset");
    host.build.installer.install.scope = zup_core::InstallScope::Either;
    let user_ledger = installed(SelectedScope::User);
    host.installations = vec![
        zup_session::ScopedInstallation {
            scope: SelectedScope::User,
            state_root: user_root.path().to_path_buf(),
            ledger: Some(user_ledger),
        },
        zup_session::ScopedInstallation {
            scope: SelectedScope::Machine,
            state_root: machine_root.path().to_path_buf(),
            ledger: None,
        },
    ];
    let seen = host.seen.clone();
    zup_session::run(
        host,
        fake_exe(),
        SessionLaunch {
            scope: Some(SelectedScope::Machine),
            interactive: true,
            ..SessionLaunch::default()
        },
    )
    .await
    .expect("scope switch installs");
    let events = seen.events.lock().expect("seen");
    let backend = events
        .iter()
        .rev()
        .find(|event| event.starts_with("backend "))
        .expect("the install reached a backend");
    assert!(
        backend.contains(&user_root.path().display().to_string()),
        "execution uses the user installation: {backend}"
    );
    assert!(
        events.iter().any(|event| event.starts_with("bind machine")),
        "planning starts from the requested machine scope"
    );
}

#[tokio::test]
async fn a_gui_uninstall_reaches_execution_without_an_install_plan() {
    let root = tempfile::tempdir().expect("state root");
    let mut host = host("uninstall_preset");
    let ledger = installed(SelectedScope::User);
    host.installations = vec![zup_session::ScopedInstallation {
        scope: SelectedScope::User,
        state_root: root.path().to_path_buf(),
        ledger: Some(ledger),
    }];
    let seen = host.seen.clone();
    let backend = host.backend.clone();
    zup_session::run(host, fake_exe(), SessionLaunch::default())
        .await
        .expect("uninstall runs");
    assert!(backend.ran.load(Ordering::SeqCst));
    let events = seen.events.lock().expect("seen");
    let record = events
        .iter()
        .rev()
        .find(|event| event.starts_with("backend "))
        .expect("uninstall reached the backend");
    assert!(
        record.contains("uninstall=true") && record.contains("plan=false"),
        "uninstall executes from the ledger, not an install plan: {record}"
    );
    assert!(record.contains(&root.path().display().to_string()));
}

#[tokio::test]
async fn a_protocol_error_during_execution_still_waits_for_settlement() {
    let hold = Hold::default();
    let mut session_host = host("protocol_fault_preset");
    session_host.backend.hold = Some(hold.clone());
    let cancelled = Arc::clone(&hold.cancelled);
    let release = Arc::clone(&hold.release);
    let session = tokio::spawn(zup_session::run(
        session_host,
        fake_exe(),
        SessionLaunch::default(),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a protocol fault cancels the running operation");
    assert!(!session.is_finished());
    release.store(true, Ordering::SeqCst);
    session
        .await
        .expect("session task")
        .expect("shutdown waits for the operation");
}

#[tokio::test]
async fn an_unexpected_operation_failure_is_reported() {
    let mut session_host = host("install_preset");
    session_host.backend.script = Script::Panic;
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        zup_session::run(session_host, fake_exe(), SessionLaunch::default()),
    )
    .await
    .expect("a panicked operation cannot hang shutdown")
    .expect_err("the session reports the lost operation");
    assert!(error.to_string().contains("ended unexpectedly"), "{error}");
}

#[tokio::test]
async fn closing_after_commit_keeps_the_committed_result() {
    let mut session_host = host("install_close_preset");
    session_host.backend.script = Script::CommitAfterCancel;
    let session = tokio::spawn(zup_session::run(
        session_host.clone(),
        fake_exe(),
        SessionLaunch::default(),
    ));
    tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("close settles")
        .expect("session task")
        .expect("a late close does not fail a committed operation");
    assert!(session_host.backend.ran.load(Ordering::SeqCst));
}

#[tokio::test]
async fn repair_uses_committed_components_despite_surface_defaults() {
    let root = tempfile::tempdir().unwrap();
    let mut session_host = host("close_preset");
    session_host.build.installer.components = [
        ("core", true, true),
        ("optional", false, true),
        ("extra", false, false),
    ]
    .into_iter()
    .map(|(id, required, default)| zup_core::Component {
        id: zup_core::ComponentId::new(id).unwrap(),
        name: zup_core::NonEmptyString::new(id).unwrap(),
        description: None,
        required,
        default,
        requires: Vec::new(),
        group: None,
    })
    .collect();
    let mut ledger = installed(SelectedScope::User);
    ledger.current.selected_components = ["core", "extra"]
        .map(|id| zup_core::ComponentId::new(id).unwrap())
        .to_vec();
    ledger.current.requirements.components = ledger.current.selected_components.clone();
    session_host.installations = vec![zup_session::ScopedInstallation {
        scope: SelectedScope::User,
        state_root: root.path().to_owned(),
        ledger: Some(ledger),
    }];
    let discovery = session_host
        .discover(fake_exe(), &SessionLaunch::default())
        .unwrap();
    let selection = zup_session::Selection {
        scope: SelectedScope::User,
        components: ["core", "optional"]
            .map(|id| zup_core::ComponentId::new(id).unwrap())
            .to_vec(),
        install_directory: Some("C:/different".into()),
    };
    for force in [false, true] {
        let completion = zup_session::complete_operation(
            &session_host,
            fake_exe(),
            &discovery,
            zup_session::OperationLaunch {
                id: uuid::Uuid::now_v7(),
                action: zup_plan::LifecycleAction::Repair { force },
                selection: &selection,
                control: zup_runtime::OperationControl::new(
                    zup_runtime::ExecutionPolicy::Interactive,
                ),
            },
        )
        .await;
        assert!(
            matches!(
                completion.outcome,
                zup_session::SessionOutcome::Deployment(zup_runtime::InstallOutcome::Committed)
            ),
            "{:?}",
            completion.outcome
        );
        assert!(session_host.backend.ran.load(Ordering::SeqCst));
    }
}

#[path = "../../zup-transaction/tests/common/mod.rs"]
mod journal_fixture;

#[test]
fn a_committed_warning_offers_only_the_owned_active_launcher() {
    let key = zup_core::ResourceKey::Launcher {
        id: zup_core::DeclaredResourceId::Application {
            id: zup_core::ResourceId::new("original-launcher").unwrap(),
        },
    };
    let mut ledger = installed(SelectedScope::User);
    ledger
        .current
        .requirements
        .launchers
        .push(zup_plan::DesiredLauncher {
            key: key.clone(),
            location: zup_core::LauncherLocation::Menu,
            launcher_path: (),
            name: zup_core::NonEmptyString::new("Original application").unwrap(),
            target: zup_core::Template::parse("C:/Session/original.exe").unwrap(),
            arguments: Vec::new(),
            working_directory: None,
            privilege: zup_core::Privilege::User,
        });
    ledger.current.resources.insert(
        key,
        zup_plan::OwnedResource::Launcher {
            launcher_path: zup_core::TargetPath::new(ledger.target, "C:/Menu/Original.lnk")
                .unwrap(),
            privilege: zup_core::Privilege::User,
            previous: zup_plan::LauncherState::Absent,
            installed: zup_plan::LauncherState::Launcher {
                target: zup_core::TargetPath::new(ledger.target, "C:/Session/original.exe")
                    .unwrap(),
                arguments: Vec::new(),
                working_directory: None,
            },
        },
    );
    let discovery = SessionDiscovery {
        build: Arc::new(fixture_build()),
        installations: Vec::new(),
        recoveries: Default::default(),
        observation_errors: Default::default(),
    };
    let mut context = OperationContext {
        scope: SelectedScope::User,
        state_root: PathBuf::from("state"),
        ledger: Some(ledger),
    };
    let mut state = zup_session::initial_state(&discovery, &context, None);
    state.finish_with(&zup_runtime::InstallOutcome::CommittedWithWarning {
        detail: "presentation refresh failed".into(),
    });
    assert_eq!(
        state.snapshot().launch.as_ref().unwrap().name,
        "Original application"
    );
    assert!(matches!(state.accept(zup_preset_protocol::Action::Launch),
        zup_session::HostDecision::Launch(target) if target.name == "Original application"));
    context.ledger.as_mut().unwrap().state = zup_plan::InstallationState::Removed;
    let mut state = zup_session::initial_state(&discovery, &context, None);
    state.finish_with(&zup_runtime::InstallOutcome::CommittedWithWarning {
        detail: "presentation refresh failed".into(),
    });
    assert!(state.snapshot().launch.is_none());
}

#[rstest::rstest]
#[case(zup_plan::LifecycleAction::Install)]
#[case(zup_plan::LifecycleAction::Upgrade)]
#[case(zup_plan::LifecycleAction::Uninstall)]
#[tokio::test]
async fn reopened_gui_finds_committed_cleanup_without_a_desired_target(
    #[case] action: zup_plan::LifecycleAction,
    #[values(false, true)] unreadable_ledger: bool,
) {
    use zup_plan::{HistoryFileTarget, HistoryObject, JournalMember, MetadataFile};
    use zup_transaction::{TransactionCoordinator, TransactionPhase, TransactionStore};
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("state");
    let store = zup_transaction::FilesystemTransactionStore::private_at(&state_root);
    let publisher = journal_fixture::Publisher::default();
    let mut input = journal_fixture::effects();
    if action != zup_plan::LifecycleAction::Install {
        let prior = TransactionCoordinator::new(store.clone())
            .begin(
                journal_fixture::sample_app_id(),
                SelectedScope::User,
                journal_fixture::sample_version(),
                zup_plan::compile_deployment(&input).unwrap(),
            )
            .unwrap()
            .execute(&mut journal_fixture::fake::FakeExecutor::new(), &publisher)
            .unwrap();
        zup_transaction::reconcile_committed(&store, &prior.record, &state_root, |_| Ok(()))
            .unwrap();
        let previous = publisher.0.lock().unwrap().clone().unwrap();
        input.observed = Some(previous.current.selection());
        input.observed_transaction = Some(previous.commit.transaction);
        if action == zup_plan::LifecycleAction::Uninstall {
            input.transition = zup_plan::DeploymentTransition::Remove;
        }
    }
    let source = zup_core::TransactionId::new_v7();
    let target = HistoryFileTarget::Collection {
        intent: zup_core::hash_bytes(b"retired collection"),
    };
    let observed = MetadataFile {
        object: journal_fixture::owned_instance().object,
        size: 1,
        sha256: zup_core::hash_bytes(b"metadata"),
    };
    let history = |object| zup_plan::CleanupOperation {
        privilege: zup_core::Privilege::User,
        candidate: zup_plan::CleanupCandidate::History(zup_plan::HistoryCleanup {
            installation: input.installation,
            source,
            materializations: Vec::new(),
            object,
        }),
    };
    input.cleanup = vec![
        history(HistoryObject::Journal {
            plan_hash: zup_core::hash_bytes(b"retired plan"),
            object: journal_fixture::owned_instance().object,
            members: [(JournalMember::Record, observed.clone())].into(),
            sidecars: vec![target.clone()],
        }),
        history(HistoryObject::File { target, observed }),
    ];
    let plan = zup_plan::compile_deployment(&input).unwrap();
    plan.validate().unwrap();
    let mut executor = journal_fixture::fake::FakeExecutor::new();
    executor.fail_verify_operation = Some(
        plan.execution_order
            .iter()
            .find(|id| {
                plan.nodes
                    .iter()
                    .any(|node| node.id == **id && node.kind.is_post_commit())
            })
            .unwrap()
            .clone(),
    );
    let settled = TransactionCoordinator::new(store.clone())
        .begin(
            journal_fixture::sample_app_id(),
            SelectedScope::User,
            journal_fixture::sample_version(),
            plan,
        )
        .unwrap()
        .execute(&mut executor, &publisher)
        .unwrap();
    assert_eq!(settled.record.phase, TransactionPhase::Committed);
    assert!(!settled.record.cleanup_complete());
    let id = settled.record.transaction_id;
    drop(store);
    let reopened = zup_transaction::FilesystemTransactionStore::private_at(&state_root);
    assert!(!reopened.load(&id).unwrap().cleanup_complete());
    let projection = reopened
        .recovery_target(&journal_fixture::sample_app_id(), SelectedScope::User, id)
        .unwrap()
        .unwrap()
        .summary();
    assert_eq!(projection.kind, zup_core::RecoveryKind::CommittedCleanup);
    assert_eq!(projection.cleanup.len(), 2);
    assert!(
        reopened
            .recovery_target(
                &journal_fixture::sample_app_id(),
                SelectedScope::Machine,
                id
            )
            .is_err()
    );
    assert!(
        reopened
            .recovery_target(
                &zup_core::AppId::new("com.example.foreign").unwrap(),
                SelectedScope::User,
                id
            )
            .is_err()
    );
    assert!(
        reopened
            .recovery_target(
                &journal_fixture::sample_app_id(),
                SelectedScope::User,
                zup_core::TransactionId::new_v7()
            )
            .is_err()
    );
    let mut session_host = host("close_preset");
    session_host.build.installer.app.id = journal_fixture::sample_app_id();
    session_host.build.installer.app.version = semver::Version::new(9, 0, 0);
    session_host.installations = vec![zup_session::ScopedInstallation {
        scope: SelectedScope::User,
        state_root,
        ledger: None,
    }];
    let before = executor
        .calls
        .iter()
        .filter(|call| !call.id.starts_with("collect:"))
        .count();
    let journal = Arc::new(JournalRecovery {
        publisher,
        executor: Mutex::new(executor),
        attempts: Default::default(),
    });
    session_host.journal = Some(journal.clone());
    session_host
        .ledger_error
        .store(unreadable_ledger, Ordering::SeqCst);
    let discovery = session_host
        .discover(fake_exe(), &SessionLaunch::default())
        .unwrap();
    let context = discovery.context(SelectedScope::User).unwrap();
    let mut state = session_host.open_state(&discovery, &context, None);
    assert_eq!(
        state.snapshot().recovery.as_ref().map(|r| r.transaction),
        Some(id)
    );
    assert_eq!(
        state.accept(zup_preset_protocol::Action::Recover { transaction: id }),
        zup_session::HostDecision::Recover { transaction: id }
    );
    assert_eq!(
        state.snapshot().settlement,
        Some(zup_core::DeploymentSettlement::Committed)
    );
    assert_eq!(
        state.snapshot().recovery.as_ref().unwrap().app,
        journal_fixture::sample_app_id()
    );
    assert_eq!(
        state.snapshot().recovery.as_ref().unwrap().version,
        journal_fixture::sample_version()
    );
    zup_session::run(
        session_host.clone(),
        fake_exe(),
        SessionLaunch {
            maintenance: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        !reopened.load(&id).unwrap().cleanup_complete(),
        "closing the GUI retains recovery evidence"
    );
    session_host.preset = host("journal_recovery_preset").preset;
    zup_session::run(session_host.clone(), fake_exe(), SessionLaunch::default())
        .await
        .unwrap();
    let final_record = reopened.load(&id).unwrap();
    assert_eq!(final_record.phase, TransactionPhase::Committed);
    assert!(final_record.cleanup_complete());
    assert_eq!(
        final_record.reconciliation,
        zup_transaction::ReconciliationState::Complete
    );
    assert_eq!(journal.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        journal
            .executor
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|call| !call.id.starts_with("collect:"))
            .count(),
        before
    );
    assert!(
        !session_host.backend.ran.load(Ordering::SeqCst),
        "the new desired deployment was never executed"
    );
}

struct JournalRecovery {
    publisher: journal_fixture::Publisher,
    executor: Mutex<journal_fixture::fake::FakeExecutor>,
    attempts: std::sync::atomic::AtomicUsize,
}

#[tokio::test]
async fn a_rollback_refresh_failure_preserves_settlement_and_has_a_safe_refresh_action() {
    let mut session_host = host("refresh_preset");
    let transaction = zup_core::TransactionId::new_v7();
    session_host.backend.script = Script::Recovery(transaction);
    session_host.refresh_failures.store(1, Ordering::SeqCst);
    zup_session::run(session_host.clone(), fake_exe(), SessionLaunch::default())
        .await
        .unwrap();
    let seen = session_host.seen.events.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|line| **line == format!("recover {transaction}"))
            .count(),
        1
    );
    assert_eq!(
        seen.iter()
            .filter(|line| line.as_str() == "refreshed")
            .count(),
        2
    );
}
