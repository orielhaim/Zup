//! The simulated machine, and the child process that draws it.
//!
//! The division here is the whole design. Everything a person is developing
//! against - the state, the settings, the files the application provided, the
//! scenario a control chose - is owned here, in a process that does not restart.
//! The preset is a child: launched, handed the current state, killed, replaced,
//! and handed the current state again. It owns nothing, which is why replacing
//! it is cheap and why the state survives its replacement.
//!
//! A new child is not adopted until it has completed the handshake. A preset that
//! compiled and a preset that starts are two different claims, and a preview that
//! treated the first as the second would replace a working window with nothing
//! whenever the new one's own startup failed.
//!
//! The scenario drives the same reducer an engine drives, by synthesising the
//! same events. A simulated state is therefore a state a real install could
//! reach, which is the property that makes development against it worth
//! anything.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use zup_core::{Sha256Digest, UiPreset};
use zup_runtime::{InstallOutcome, RuntimeEvent};
use zup_ui_host::HostState;
use zup_ui_protocol::{
    ComponentOption, InstallOptions, InstallScope, InstallationHealth, MaintenanceState,
    ProductIdentity, UiAction, UiCapabilities, UiCapability, UiConfiguration, UiSnapshot,
    UpdateState,
};

use crate::state::StateDirectory;

/// Which surface the simulated machine is presenting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// A machine that has never had this application.
    Install,
    /// A machine that has, and can therefore be modified, repaired or removed.
    Maintenance,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Maintenance => "maintenance",
        }
    }
}

/// The machine being simulated.
///
/// Every field here is something a real machine has an answer for. Nothing is a
/// preview-only concept, because a state a preset can only meet in a preview is
/// a state its author will believe in and its users will never see.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub product: ProductIdentity,
    pub surface: Surface,
    pub scopes: Vec<InstallScope>,
    pub scope: InstallScope,
    pub components: Vec<ComponentOption>,
    pub install_directory: String,
    pub allow_directory_override: bool,
    pub existing_version: Option<String>,
    pub installed_version: String,
    pub updates_enabled: bool,
    /// What the update channel says, when the preset asks.
    pub update: UpdateState,
    /// What a repair found, when a repair has run.
    pub health: InstallationHealth,
    pub drift: Vec<String>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            product: ProductIdentity {
                name: "Acme".into(),
                publisher: Some("Acme Inc".into()),
                version: "1.4.0".into(),
                description: Some("The Acme application.".into()),
            },
            surface: Surface::Install,
            scopes: vec![InstallScope::User, InstallScope::Machine],
            scope: InstallScope::User,
            components: Vec::new(),
            install_directory: "C:\\Program Files\\Acme".into(),
            allow_directory_override: true,
            existing_version: None,
            installed_version: "1.3.0".into(),
            updates_enabled: true,
            update: UpdateState::Idle,
            health: InstallationHealth::Unknown,
            drift: Vec::new(),
        }
    }
}

impl Scenario {
    /// The machine a real application would present, from its own compiled form.
    ///
    /// Every field is derived by the functions a real host uses to open a
    /// session, so the first thing a person sees is the application they are
    /// authoring rather than a demonstration. What a real machine has that this
    /// does not - an existing installation, a health reading, an update channel -
    /// is exactly what the controls are for, and it starts as unknown rather than
    /// as an answer nobody has.
    pub fn from_installer(installer: &zup_core::Installer) -> Self {
        let directory = installer
            .install
            .directory
            .user
            .as_ref()
            .or(installer.install.directory.machine.as_ref())
            .map(ToString::to_string)
            .unwrap_or_default();
        Self {
            product: zup_ui_host::product(installer),
            scopes: zup_ui_host::scopes(installer),
            scope: zup_ui_host::scope(zup_ui_host::default_scope(installer)),
            components: zup_ui_host::surface_components(installer, None, None),
            install_directory: directory,
            allow_directory_override: installer.install.allow_directory_override,
            updates_enabled: installer.updates.is_some(),
            ..Self::default()
        }
    }

    /// What this machine can offer a preset.
    ///
    /// Derived over the same capabilities an installer's answer is derived from,
    /// so a preset is refused here for the same reason it would be refused there.
    pub fn capabilities(&self) -> UiCapabilities {
        let mut capabilities = UiCapabilities::new([
            UiCapability::Diagnostics,
            UiCapability::PlanPreview,
            UiCapability::InstallDirectory,
        ]);
        if !self.components.is_empty() {
            capabilities = capabilities.with(UiCapability::Components);
        }
        if self.updates_enabled {
            capabilities = capabilities.with(UiCapability::Updates);
        }
        capabilities.with(UiCapability::Maintenance)
    }

    /// The state a session on this machine opens in.
    pub fn host(&self) -> HostState {
        match self.surface {
            Surface::Install => HostState::install(
                self.product.clone(),
                InstallOptions {
                    existing_version: self.existing_version.clone(),
                    scopes: self.scopes.clone(),
                    scope: self.scope,
                    components: self.components.clone(),
                    install_directory: (!self.install_directory.is_empty())
                        .then(|| self.install_directory.clone()),
                    allow_directory_override: self.allow_directory_override,
                },
                self.capabilities(),
            ),
            Surface::Maintenance => HostState::maintenance(
                self.product.clone(),
                MaintenanceState {
                    installed_version: self.installed_version.clone(),
                    components: self.components.clone(),
                    updates_enabled: self.updates_enabled,
                    scope: self.scope,
                    install_directory: (!self.install_directory.is_empty())
                        .then(|| self.install_directory.clone()),
                    health: self.health.clone(),
                },
                self.capabilities(),
            ),
        }
    }
}

/// Why a selection could not become a running child.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error("this host cannot present the preset: {0}")]
    Incompatible(String),
    #[error("the preset could not be staged: {0}")]
    Copy(String),
}

/// The running preview session.
///
/// Holds the state, the configuration, and at most one preset child. Every
/// mutation of the state goes through [`Simulator::act`] or [`Simulator::observe`],
/// which are the same two entry points the child's own actions and a real
/// engine's events take, so a control cannot put the simulated machine somewhere
/// a preset could not have put it.
pub struct Simulator {
    state: StateDirectory,
    host: HostState,
    configuration: UiConfiguration,
    child: Option<zup_ui_host::PresetProcess>,
    generation: u64,
}

impl Simulator {
    /// A session with no child yet, which is the state before anything is shown.
    pub fn new(state: StateDirectory, scenario: Scenario) -> Self {
        let host = scenario.host();
        let configuration = UiConfiguration {
            settings: serde_json::json!({}),
            assets: BTreeMap::new(),
        };
        Self {
            state,
            host,
            configuration,
            child: None,
            generation: 0,
        }
    }

    /// The state a preset would see right now.
    pub fn snapshot(&self) -> &UiSnapshot {
        self.host.snapshot()
    }

    /// What the preset is told the application configured.
    pub fn configuration(&self) -> &UiConfiguration {
        &self.configuration
    }

    /// Where this session keeps what it made.
    pub fn state(&self) -> &StateDirectory {
        &self.state
    }

    /// Reopen the session on a different machine.
    ///
    /// A surface change is not a `UiAction`: choosing between a fresh install
    /// and a maintenance session is a fact about the machine, and the protocol
    /// has no action for it because a preset cannot decide it. The state machine
    /// is rebuilt from the new scenario and everything the host still owns - the
    /// settings, the files, the running child - is kept, so switching surfaces
    /// does not close the window.
    pub fn reopen(&mut self, scenario: &Scenario) {
        self.host = scenario.host();
    }

    /// The generation currently running, if any.
    pub fn generation(&self) -> Option<u64> {
        self.child.as_ref().map(|_| self.generation)
    }

    /// Whether the child is still alive.
    ///
    /// Checks the process rather than assuming, because a preset that crashes
    /// must be noticed: the session survives it, and the next selection
    /// replaces it.
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => child.is_running(),
            None => false,
        }
    }

    /// Forget the child, having noticed it exited, and end it.
    ///
    /// Reaping here rather than at exit is what keeps a crashed preset from
    /// becoming a permanent one: the handle is waited on, so the process is gone
    /// before anything else is started.
    pub fn forget_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.shutdown();
        }
    }

    /// Copy a verified selection somewhere it can be launched from, and check
    /// this host could present it at all.
    ///
    /// The copy is what makes replacement possible: a running executable cannot
    /// be overwritten on the platform this product targets, so each generation
    /// gets its own file and an earlier one is only removed once nothing is
    /// running from it.
    pub fn stage(&self, bytes: &[u8], preset: &UiPreset) -> Result<Candidate, StageError> {
        zup_ui_host::process::check_presentable(preset, self.host.capabilities())
            .map_err(StageError::Incompatible)?;
        let generation = self.generation + 1;
        write_durable(&self.state.run_executable(generation), bytes)
            .map_err(|error| StageError::Copy(error.to_string()))?;
        Ok(Candidate {
            executable: self.state.run_executable(generation),
            generation,
        })
    }

    /// Start a child and, only once it has opened the session, make it the one
    /// that is running.
    ///
    /// `asked` is where the child's actions arrive. The reader is one thread
    /// because a session has one reading half, and a second reader would make the
    /// protocol's sequence numbers describe two interleavings instead of one.
    pub fn adopt(
        &mut self,
        candidate: Candidate,
        asked: Sender<UiAction>,
    ) -> Result<(), zup_ui_host::SessionError> {
        let mut process = zup_ui_host::launch(
            &candidate.executable,
            self.host.capabilities().clone(),
            self.host.snapshot().product.clone(),
        )?;
        // The state goes out before the reader starts, so a child that draws the
        // instant it can draws the real one rather than an empty window.
        if let Err(error) = process.publish(
            self.configuration.clone(),
            Box::new(self.host.snapshot().clone()),
        ) {
            process.shutdown();
            return Err(error);
        }
        let reader = process.take_reader();
        std::thread::Builder::new()
            .name("zup-preview-preset".into())
            .spawn(move || {
                while let Some(action) = reader.next() {
                    if asked.send(action).is_err() {
                        return;
                    }
                }
            })
            .map_err(|error| zup_ui_host::SessionError::Handshake(error.to_string()))?;

        // Only now is the previous child finished with. Until this point the
        // working window was still up, and a failure above leaves it up.
        if let Some(mut previous) = self.child.take() {
            previous.shutdown();
        }
        self.generation = candidate.generation;
        self.child = Some(process);
        self.retire();
        Ok(())
    }

    /// Feed one action to the state machine, whether a preset or a control sent
    /// it.
    pub fn act(&mut self, action: UiAction) -> zup_ui_host::HostDecision {
        self.host.accept(action)
    }

    /// Report an engine event to the state machine.
    ///
    /// Named for what a real host does, because this is the only way the simulated
    /// state moves forward: a development control causes the same event a real
    /// operation would rather than setting a state of its own.
    pub fn observe(&mut self, event: &RuntimeEvent) {
        self.host.observe(event);
    }

    /// Report the outcome of a finished transaction.
    pub fn finish_with(&mut self, outcome: &InstallOutcome) {
        self.host.finish_with(outcome);
    }

    /// Record what the update channel says.
    pub fn set_update(&mut self, state: UpdateState) {
        self.host.set_update(Some("stable".into()), state);
    }

    /// Record the resources a repair found drifted.
    pub fn set_drift(&mut self, resources: Vec<String>) {
        self.host.set_repair_drift(resources);
    }

    /// Send the current state to the running child.
    pub fn publish(&self) -> Result<(), zup_ui_host::SessionError> {
        let Some(child) = self.child.as_ref() else {
            return Ok(());
        };
        child.publish(
            self.configuration.clone(),
            Box::new(self.host.snapshot().clone()),
        )
    }

    /// Replace the settings the preset is told.
    ///
    /// No check here, because the schema a caller validates against is the one
    /// that chose these settings: a preset author has the schema their own preset
    /// generated, and an application author has the schema the package carried.
    /// The runtime holds neither, and a check it could perform would be its own
    /// answer to a question somebody else already answered.
    pub fn set_settings(&mut self, settings: serde_json::Value) {
        self.configuration.settings = settings;
    }

    /// Materialize one application-provided asset, and record where it is.
    ///
    /// Content-addressed for the same reason an installation's are: a file that
    /// changed is a new path, so a preset that already read the old one is not
    /// looking at stale content the next time it asks.
    pub fn set_asset(&mut self, name: &str, source: &Path) -> Result<Sha256Digest, String> {
        let bytes =
            std::fs::read(source).map_err(|error| format!("{}: {error}", source.display()))?;
        let digest = zup_core::hash_bytes(&bytes);
        let path = self.state.asset_directory(digest);
        if !path.is_file() {
            write_durable(&path, &bytes).map_err(|error| error.to_string())?;
        }
        self.configuration.assets.insert(
            name.to_owned(),
            path.clone()
                .into_os_string()
                .into_string()
                .unwrap_or_default(),
        );
        Ok(digest)
    }

    /// Forget every asset the application was providing.
    pub fn clear_assets(&mut self) {
        self.configuration.assets.clear();
    }

    /// End the session, and take the child with it.
    pub fn shutdown(&mut self) {
        self.forget_child();
        self.state.retire(None);
    }

    /// Remove the run directories of generations that are no longer running.
    fn retire(&self) {
        self.state
            .retire(self.child.as_ref().map(|_| self.generation));
    }
}

/// A build product that has been copied somewhere it can be launched from.
#[derive(Debug)]
pub struct Candidate {
    /// Where the executable was copied for this generation.
    pub executable: PathBuf,
    /// The generation it belongs to, so a stale one can be recognised.
    pub generation: u64,
}

/// Write a file so that a reader never sees a half-written one.
///
/// A temporary beside it and a rename over it, which is the same durability the
/// rest of this repository asks for and the reason a preset that reads a
/// just-materialized asset reads a whole one. The temporary's name is unique per
/// call, because two writers racing on one name is a way for one of them to
/// rename a file the other is still writing.
fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITES: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.to_path_buf();
    temporary.set_extension(format!(
        "{}.{}",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}
