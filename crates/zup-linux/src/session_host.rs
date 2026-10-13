use std::path::{Path, PathBuf};
use std::sync::Arc;

use zup_core::SelectedScope;
use zup_runtime::{PreparedOperation, RuntimeBackend};
use zup_session::{SessionDiscovery, SessionHost, SessionLaunch};

use crate::operation_host::LinuxOperationHost;

#[derive(Clone, Default)]
pub struct LinuxSessionHost {
    installer: PathBuf,
    scope: Option<SelectedScope>,
    state_root: Option<PathBuf>,
}

impl LinuxSessionHost {
    pub fn open(installer: PathBuf) -> Self {
        Self {
            installer,
            scope: None,
            state_root: None,
        }
    }

    fn operation_host(&self) -> Result<LinuxOperationHost, String> {
        let machine = match self.scope {
            Some(SelectedScope::Machine) => Some(crate::operation_host::MachineHost {
                roots: crate::locations::MachineRoots::production(),
                systemd: crate::machine::SystemdRoots::production(),
            }),
            _ => None,
        };
        Ok(LinuxOperationHost { machine })
    }
}

impl zup_runtime::OperationHost for LinuxSessionHost {
    fn check_capabilities(&self, input: &zup_runtime::OperationInput) -> Result<(), String> {
        self.operation_host()?.check_capabilities(input)
    }

    fn plan_build(
        &self,
        build: &zup_core::TargetBuildPlan,
        request: &zup_plan::PlanRequest,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PlannedInstallation, String> {
        self.operation_host()?
            .plan_build(build, request, cancellation)
    }

    fn resolve_target(
        &self,
        install: &zup_plan::InstallPlan,
    ) -> Result<zup_platform::TargetPlan, String> {
        self.operation_host()?.resolve_target(install)
    }

    fn resolve_target_for_uninstall(&self) -> bool {
        true
    }

    fn install_directory(
        &self,
        explicit: Option<&std::path::Path>,
        prior: Option<&zup_exec::InstallLedger>,
        allowed: bool,
        repair: bool,
    ) -> Result<Option<zup_core::Template>, String> {
        self.operation_host()?
            .install_directory(explicit, prior, allowed, repair)
    }

    fn observe(
        &self,
        target: Option<&zup_platform::TargetPlan>,
        ledger: Option<&zup_exec::InstallLedger>,
    ) -> Result<
        (
            Option<zup_exec::HostSnapshot>,
            std::collections::BTreeMap<zup_core::ResourceKey, bool>,
        ),
        String,
    > {
        self.operation_host()?.observe(target, ledger)
    }

    fn compile(
        &self,
        action: zup_exec::LifecycleAction,
        execution: &zup_exec::ExecutionPlan,
        target: Option<&zup_platform::TargetPlan>,
        ledger: Option<&zup_exec::InstallLedger>,
    ) -> Result<zup_transaction::TransactionInput, String> {
        self.operation_host()?
            .compile(action, execution, target, ledger)
    }

    fn attach_content(
        &self,
        target: &mut zup_platform::TargetPlan,
        content: &zup_runtime::ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        self.operation_host()?
            .attach_content(target, content, preset)
    }
}

impl SessionHost for LinuxSessionHost {
    fn discover(
        &mut self,
        executable: &Path,
        launch: &SessionLaunch,
    ) -> Result<SessionDiscovery, String> {
        self.installer = executable.to_path_buf();
        let carrier =
            crate::carrier::Carrier::open(executable).map_err(|error| error.to_string())?;
        let build = carrier
            .package()
            .build_plan()
            .map_err(|error| error.to_string())?
            .targets
            .into_iter()
            .next()
            .ok_or_else(|| "an installer package holds exactly one target".to_owned())?;
        let scope = zup_runtime::resolve_scope(build.installer.install.scope, launch.scope)
            .map_err(|error| error.to_string())?;
        let state_root = match &launch.state_root {
            Some(root) => root.clone(),
            None => crate::paths::state_root(scope).map_err(|error| error.to_string())?,
        };
        let mut priors = Vec::new();
        for candidate in [SelectedScope::User, SelectedScope::Machine] {
            let root = if candidate == scope {
                state_root.clone()
            } else if launch.state_root.is_some() {
                state_root.clone()
            } else {
                crate::paths::state_root(candidate).map_err(|error| error.to_string())?
            };
            let ledger = crate::ledger::LinuxLedgerStore::new(&root)
                .load(&build.installer.app.id, candidate)
                .map_err(|error| error.to_string())?;
            if let Some(ledger) = ledger {
                priors.push((candidate, ledger));
            }
        }
        self.scope = Some(scope);
        self.state_root = Some(state_root.clone());
        Ok(SessionDiscovery {
            build,
            priors,
            scope,
            state_root,
        })
    }

    fn backend(&self, prepared: &PreparedOperation) -> Result<Arc<dyn RuntimeBackend>, String> {
        let state_root = self
            .state_root
            .clone()
            .ok_or_else(|| "the session host was not discovered".to_owned())?;
        let target = prepared
            .target_plan
            .as_ref()
            .ok_or_else(|| "a Linux transaction resolves its target".to_owned())?;
        let generated = match prepared.install_plan.as_ref() {
            Some(install) => {
                let generated = crate::integration::generated_map(install)
                    .map_err(|error| error.to_string())?;
                crate::integration::save_generated(
                    &state_root,
                    &install.app.id,
                    target.scope,
                    &generated,
                )
                .map_err(|error| error.to_string())?;
                generated
            }
            None => crate::integration::load_generated(&state_root, &target.app.id, target.scope)
                .map_err(|error| error.to_string())?,
        };
        Ok(Arc::new(crate::backend::LinuxRuntimeBackend {
            installer: self.installer.clone(),
            operation: prepared.operation,
            selection: prepared.selection.clone(),
            install_dir_override: prepared.selection.install_directory.clone(),
            generated,
        }))
    }

    fn materialize_preset(
        &self,
        _discovery: &SessionDiscovery,
    ) -> Result<zup_session::PresetLaunch, String> {
        Err("the graphical installer is not supported on Linux".into())
    }
}
