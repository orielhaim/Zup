use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use zup_core::{SelectedScope, Target};
use zup_session::{SessionDiscovery, SessionHost, SessionLaunch};

#[derive(Clone)]
struct FakeBackend {
    ran: Arc<AtomicBool>,
}

impl zup_runtime::RuntimeBackend for FakeBackend {
    fn payload_source(
        &self,
        _request: &zup_runtime::RuntimeRequest,
    ) -> zup_runtime::RuntimePayloadSource {
        let root = tempfile::tempdir().expect("payload root");
        let root = root.keep();
        Arc::new(zup_bundle::DirectoryPayloadSource::new(root))
    }

    fn execute<'a>(
        &'a self,
        _request: zup_runtime::RuntimeRequest,
        _control: zup_runtime::RuntimeControl,
    ) -> zup_runtime::RuntimeFuture<
        'a,
        Result<zup_runtime::InstallOutcome, zup_runtime::SessionError>,
    > {
        Box::pin(async move {
            self.ran.store(true, Ordering::SeqCst);
            Ok(zup_runtime::InstallOutcome::Committed)
        })
    }
}

#[derive(Clone)]
struct FakeHost {
    build: zup_core::TargetBuildPlan,
    backend: FakeBackend,
    preset: PathBuf,
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

fn fixture_target() -> zup_platform::TargetPlan {
    let target = Target::parse("windows-x64").expect("valid target");
    zup_platform::TargetPlan {
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
        install_directory: zup_platform::TargetPath::new(target, "C:\\Session")
            .expect("valid path"),
        selected_components: Vec::new(),
        prerequisites: Vec::new(),
        files: Vec::new(),
        launchers: Vec::new(),
        path_entries: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
        summary: zup_platform::TargetPlanSummary {
            file_count: 0,
            install_bytes: 0,
            resource_count: 0,
            requires_authorization: false,
            selected_component_count: 0,
            prerequisite_count: 0,
            download_bytes: 0,
        },
        preset: None,
    }
}

fn fixture_install() -> zup_plan::InstallPlan {
    zup_plan::InstallPlan {
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
        summary: zup_plan::PlanSummary {
            file_count: 0,
            install_bytes: 0,
            selected_component_count: 0,
            resource_count: 0,
            requires_authorization: false,
            prerequisite_count: 0,
            download_bytes: 0,
        },
    }
}

impl zup_runtime::OperationHost for FakeHost {
    fn plan_build(
        &self,
        _build: &zup_core::TargetBuildPlan,
        _request: &zup_plan::PlanRequest,
        _cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PlannedInstallation, String> {
        Ok(zup_plan::PlannedInstallation {
            plan: fixture_install(),
            generated_files: Vec::new(),
        })
    }

    fn resolve_target(
        &self,
        _install: &zup_plan::InstallPlan,
    ) -> Result<zup_platform::TargetPlan, String> {
        Ok(fixture_target())
    }

    fn observe(
        &self,
        _target: Option<&zup_platform::TargetPlan>,
        _ledger: Option<&zup_exec::InstallLedger>,
    ) -> Result<
        (
            Option<zup_exec::HostSnapshot>,
            BTreeMap<zup_core::ResourceKey, bool>,
        ),
        String,
    > {
        Ok((Some(zup_exec::HostSnapshot::default()), BTreeMap::new()))
    }

    fn compile(
        &self,
        _action: zup_exec::LifecycleAction,
        _execution: &zup_exec::ExecutionPlan,
        _target: Option<&zup_platform::TargetPlan>,
        _ledger: Option<&zup_exec::InstallLedger>,
    ) -> Result<zup_transaction::TransactionInput, String> {
        Ok(zup_transaction::TransactionInput::new(
            Target::parse("windows-x64").expect("valid target"),
        ))
    }

    fn attach_content(
        &self,
        _target: &mut zup_platform::TargetPlan,
        _content: &zup_runtime::ContentAccess,
        _preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        Ok(())
    }
}

impl SessionHost for FakeHost {
    fn discover(
        &mut self,
        _executable: &std::path::Path,
        _launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        let root = tempfile::tempdir().expect("state root");
        Ok(SessionDiscovery {
            build: self.build.clone(),
            priors: Vec::new(),
            scope: SelectedScope::User,
            state_root: root.keep(),
        })
    }

    fn backend(
        &self,
        _prepared: &zup_runtime::PreparedOperation,
    ) -> Result<Arc<dyn zup_runtime::RuntimeBackend>, String> {
        Ok(Arc::new(self.backend.clone()))
    }

    fn materialize_preset(
        &self,
        discovery: &SessionDiscovery,
    ) -> Result<zup_session::PresetLaunch, String> {
        Ok(zup_session::PresetLaunch {
            executable: self.preset.clone(),
            configuration: zup_preset_protocol::Configuration {
                settings: serde_json::json!({}),
                assets: BTreeMap::new(),
            },
            capabilities: zup_preset_host::surface::capabilities(discovery.installer(), false),
            product: zup_preset_host::surface::product(discovery.installer()),
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
        },
        preset: stub,
    }
}

fn fake_exe() -> &'static Path {
    Path::new("session-test")
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
