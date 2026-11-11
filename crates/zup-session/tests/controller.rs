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
        transaction: zup_core::TransactionId,
        _control: &'a zup_runtime::OperationControl,
    ) -> zup_runtime::RuntimeFuture<
        'a,
        Result<zup_runtime::InstallOutcome, zup_runtime::SessionError>,
    > {
        Box::pin(async move {
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
        Ok(None)
    }

    fn observe(
        &self,
        _installer: &zup_core::Installer,
        scope: SelectedScope,
        state_root: Option<&Path>,
    ) -> Result<OperationContext, String> {
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

    fn discover(
        &mut self,
        _executable: &std::path::Path,
        _launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        let installations = if self.installations.is_empty() {
            let root = tempfile::tempdir().expect("state root");
            vec![zup_session::ScopedInstallation {
                scope: SelectedScope::User,
                state_root: root.keep(),
                ledger: None,
            }]
        } else {
            self.installations.clone()
        };
        Ok(SessionDiscovery {
            build: Arc::new(self.build.clone()),
            installations,
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

    fn after_commit(
        &self,
        _context: &OperationContext,
        _prepared: &zup_runtime::PreparedOperation,
    ) -> Result<(), String> {
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
        installations: Vec::new(),
        seen: Seen::default(),
        refuse_backend: false,
        after_commit_error: None,
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
                zup_runtime::InstallOutcome::CommittedPendingReconciliation { .. }
            )
        ),
        "cleanup after commit stays a committed reconciliation: {:?}",
        completion.outcome
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
    )
    .expect("presentation");
    assert_eq!(scope, SelectedScope::Machine);
    let headless = zup_session::select_scope(
        zup_core::InstallScope::Either,
        &SessionLaunch::default(),
        &[],
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
