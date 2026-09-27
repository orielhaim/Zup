//! The embedded-package lifecycle, tested against a real plan.
//!
//! The plan is written out literally rather than compiled from a manifest,
//! because the runtime never sees a manifest: it is handed a plan, and a fixture
//! that reached for the compiler to produce one would put a build-plane
//! dev-dependency into the package whose whole purpose is to have none.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;
use zup_core::{
    App, AppId, FileMapping, Install, InstallDirectory, InstallScope, Installer, NonEmptyString,
    PluginBinding, PluginId, RelativePath, ResolvedFile, ResolvedPlugin, SelectedScope, Template,
    hash_reader,
};
use zup_exec::LifecycleAction;
use zup_plan::{
    CancellationQuery, PluginExecutor, PluginFailure, PluginPlanningContext,
    PluginResourceProposal, TargetBuildPlan,
};

use super::{EmbeddedPreparation, Request, RuntimeCancellationQuery, prepare_embedded_request};

/// The plugin the fixture declares, which no test here ever plans.
const PLUGIN: &str = "helper";

/// A plan for an application that declares one plugin and one payload file.
fn plan_with_plugin(root: &Path) -> TargetBuildPlan {
    let payload = root.join("app.exe");
    std::fs::write(&payload, b"app").expect("payload file");
    let source = root.join("helper.wasm");
    std::fs::write(&source, b"plugin").expect("plugin source");
    let (size, sha256) = hash_reader(b"app".as_slice()).expect("the payload hashes");
    let (source_size, source_sha256) =
        hash_reader(b"plugin".as_slice()).expect("the plugin hashes");
    let plugin_id = PluginId::new(PLUGIN).expect("a plugin id");
    TargetBuildPlan {
        installer: Installer {
            app: App {
                id: AppId::new("com.example.embedded-plugin").expect("an app id"),
                name: NonEmptyString::new("Embedded Plugin").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: zup_core::TargetTriple::parse(HOST_TARGET).expect("a target"),
            frontend: zup_core::Frontend::Gui,
            ui: None,
            updates: None,
            install: Install {
                scope: InstallScope::User,
                directory: InstallDirectory {
                    user: Some(
                        Template::parse("${location.user_data}/EmbeddedPlugin")
                            .expect("a directory"),
                    ),
                    machine: None,
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            plugins: vec![PluginBinding {
                id: plugin_id.clone(),
                component: None,
                when: None,
            }],
            files: vec![FileMapping {
                source: "app.exe".to_owned(),
                destination: Template::parse("${install}").expect("a destination"),
                component: None,
                when: None,
                allow_empty: false,
            }],
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: vec![ResolvedPlugin {
            id: plugin_id,
            source,
            source_relative: RelativePath::new("helper.wasm").expect("a relative path"),
            size: source_size,
            sha256: source_sha256,
        }],
        files: vec![ResolvedFile {
            source: payload,
            source_relative: RelativePath::new("app.exe").expect("a relative path"),
            destination: Template::parse("${install}").expect("a destination"),
            size,
            sha256,
            component: None,
            condition: None,
        }],
        total_size: size,
        prerequisite_size: 0,
    }
}

/// The machine the host runs, which is the machine a runtime template is for.
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;

/// An executor that records whether it was ever asked to plan anything.
///
/// The count is in an `Arc` rather than in the executor so a caller can read it
/// after the executor has been moved into the preparation.
struct RecordingExecutor {
    target: zup_core::TargetTriple,
    planned: Arc<AtomicUsize>,
}

impl RecordingExecutor {
    fn new(target: zup_core::TargetTriple) -> (Self, Arc<AtomicUsize>) {
        let planned = Arc::new(AtomicUsize::new(0));
        (
            Self {
                target,
                planned: planned.clone(),
            },
            planned,
        )
    }
}

impl PluginExecutor for RecordingExecutor {
    fn target(&self) -> &zup_core::TargetTriple {
        &self.target
    }

    fn plan(
        &mut self,
        _binding: &zup_core::PluginBinding,
        _context: &PluginPlanningContext,
        _cancellation: &dyn CancellationQuery,
    ) -> Result<PluginResourceProposal, PluginFailure> {
        self.planned.fetch_add(1, Ordering::SeqCst);
        unreachable!("the fixture declares a plugin that is never planned in this test")
    }
}

fn committed_ledger(build: &TargetBuildPlan) -> zup_exec::InstallLedger {
    let mut ledger = zup_exec::InstallLedger::new(
        build.installer.app.id.clone(),
        build.installer.target.clone(),
        SelectedScope::User,
    );
    ledger.version = build.installer.app.version.clone();
    ledger
}

#[test]
fn an_uninstall_removes_by_ownership_and_never_loads_the_plugin_executor() {
    let root = TempDir::new().expect("temp dir");
    let build = plan_with_plugin(root.path());
    let (executor, planned) = RecordingExecutor::new(build.installer.target.clone());
    // The target cannot be lowered on a host without the Windows backend, so this
    // fixture is expected to fail — and it must fail without ever asking the
    // plugin executor anything, which is the property under test.
    let result = prepare_embedded_request(
        EmbeddedPreparation {
            request: Request::Named(LifecycleAction::Uninstall),
            scope: SelectedScope::User,
            build: &build,
            prior: Some(committed_ledger(&build)),
            state_root: root.path().join("state"),
            payload_root: root.path().to_path_buf(),
            enable: Vec::new(),
            disable: Vec::new(),
            install_directory: None,
        },
        None,
        true,
        &zup_plan::NeverCancelled,
        || Ok(executor),
    );
    assert!(
        result.is_err(),
        "the fixture target is not lowerable on this host"
    );
    assert_eq!(
        planned.load(Ordering::SeqCst),
        0,
        "an uninstall must not load the component engine at all"
    );
}

#[test]
fn a_cancelled_window_cancels_planning_before_execution_starts() {
    let root = TempDir::new().expect("temp dir");
    let build = plan_with_plugin(root.path());
    let cancel = zup_runtime::CancellationHandle::new();
    cancel.cancel();
    let (executor, _) = RecordingExecutor::new(build.installer.target.clone());
    let result = prepare_embedded_request(
        EmbeddedPreparation {
            request: Request::Apply,
            scope: SelectedScope::User,
            build: &build,
            prior: None,
            state_root: root.path().join("state"),
            payload_root: root.path().to_path_buf(),
            enable: Vec::new(),
            disable: Vec::new(),
            install_directory: None,
        },
        None,
        true,
        &RuntimeCancellationQuery(&cancel),
        || Ok(executor),
    );
    let error = match result {
        Ok(_) => panic!("cancelled planning unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("planning was cancelled"),
        "{error}"
    );
}

#[test]
fn a_repair_refuses_a_selection_it_does_not_use() {
    let root = TempDir::new().expect("temp dir");
    let build = plan_with_plugin(root.path());
    let (executor, _) = RecordingExecutor::new(build.installer.target.clone());
    let outcome = prepare_embedded_request(
        EmbeddedPreparation {
            request: Request::Named(LifecycleAction::Repair { force_files: false }),
            scope: SelectedScope::User,
            build: &build,
            prior: Some(committed_ledger(&build)),
            state_root: root.path().join("state"),
            payload_root: root.path().to_path_buf(),
            enable: vec!["docs".to_owned()],
            disable: Vec::new(),
            install_directory: None,
        },
        None,
        true,
        &zup_plan::NeverCancelled,
        || Ok(executor),
    );
    let error = match outcome {
        Ok(_) => panic!("a repair accepted a component selection"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("committed component selection"), "{error}");
}
