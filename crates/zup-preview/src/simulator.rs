//! The child process that draws the simulated machine.
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

use zup_core::{PresetRuntime, Sha256Digest};
use zup_preset_protocol::{Action, Configuration, Snapshot, UpdateState};
use zup_runtime::{InstallOutcome, RuntimeEvent};

use crate::machine::{Machine, Scenario};
use crate::state::StateDirectory;

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
    machine: Machine,
    configuration: Configuration,
    child: Option<zup_preset_host::PresetProcess>,
    generation: u64,
}

impl Simulator {
    /// A session with no child yet, which is the state before anything is shown.
    pub fn new(state: StateDirectory, scenario: Scenario) -> Self {
        let configuration = Configuration {
            settings: serde_json::json!({}),
            assets: BTreeMap::new(),
        };
        Self {
            state,
            machine: Machine::new(scenario),
            configuration,
            child: None,
            generation: 0,
        }
    }

    /// The state a preset would see right now.
    pub fn snapshot(&self) -> &Snapshot {
        self.machine.snapshot()
    }

    /// The simulated machine, for a control that drives it directly.
    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    /// The machine the engine is running against.
    pub fn scenario(&self) -> &Scenario {
        self.machine.scenario()
    }

    /// What the preset is told the application configured.
    pub fn configuration(&self) -> &Configuration {
        &self.configuration
    }

    /// Where this session keeps what it made.
    pub fn state(&self) -> &StateDirectory {
        &self.state
    }

    /// Reopen the session on a different machine.
    ///
    /// A surface change is not a `Action`: choosing between a fresh install
    /// and a maintenance session is a fact about the machine, and the protocol
    /// has no action for it because a preset cannot decide it. The state machine
    /// is rebuilt from the new scenario and everything the host still owns - the
    /// settings, the files, the running child - is kept, so switching surfaces
    /// does not close the window.
    pub fn reopen(&mut self, scenario: &Scenario) {
        self.machine.reopen(scenario);
    }

    /// The generation currently running, if any.
    pub fn generation(&self) -> Option<u64> {
        self.child.as_ref().map(|_| self.generation)
    }

    /// Whether the child is still alive.
    ///
    /// Checks the process rather than assuming, because a window that has
    /// closed has to end the preview rather than leave a session with nothing
    /// to show.
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => child.is_running(),
            None => false,
        }
    }

    /// Forget the child, having noticed it exited, and end it.
    ///
    /// Reaping here rather than at exit is what keeps a crashed preset from
    /// becoming a permanent one: the whole tree is ended, so the process and
    /// anything it started are gone before anything else is started.
    pub fn forget_child(&mut self) {
        drop(self.child.take());
    }

    /// Copy a verified selection somewhere it can be launched from, and check
    /// this host could present it at all.
    ///
    /// The copy is what makes replacement possible: a running executable cannot
    /// be overwritten on the platform this product targets, so each generation
    /// gets its own file and an earlier one is only removed once nothing is
    /// running from it.
    pub fn stage(&self, bytes: &[u8], preset: &PresetRuntime) -> Result<Candidate, StageError> {
        zup_preset_host::process::check_presentable(preset, self.machine.capabilities())
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
        asked: Sender<Action>,
    ) -> Result<(), zup_preset_host::SessionError> {
        let mut process = zup_preset_host::launch(
            &candidate.executable,
            self.machine.capabilities().clone(),
            self.machine.snapshot().product.clone(),
        )?;
        // The state goes out before the reader starts, so a child that draws the
        // instant it can draws the real one rather than an empty window. A
        // failure here drops `process`, which ends the tree it started.
        process.publish(
            self.configuration.clone(),
            Box::new(self.machine.snapshot().clone()),
        )?;
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
            .map_err(|error| zup_preset_host::SessionError::Handshake(error.to_string()))?;

        // Only now is the previous child finished with. Until this point the
        // working window was still up, and a failure above leaves it up. The tree
        // goes with it: a preset that has been replaced must not leave helpers of
        // its own running against the executable the next generation is staging.
        drop(self.child.take());
        self.generation = candidate.generation;
        self.child = Some(process);
        self.retire();
        Ok(())
    }

    /// Feed one action to the state machine, whether a preset or a control sent
    /// it.
    pub fn act(&mut self, action: Action) -> zup_preset_host::HostDecision {
        self.machine.act(action)
    }

    /// Report an engine event to the state machine.
    ///
    /// Named for what a real host does, because this is the only way the simulated
    /// state moves forward: a development control causes the same event a real
    /// operation would rather than setting a state of its own.
    pub fn observe(&mut self, event: &RuntimeEvent) {
        self.machine.observe(event);
    }

    /// Report the outcome of a finished transaction.
    pub fn finish_with(&mut self, outcome: &InstallOutcome) {
        self.machine.finish_with(outcome);
    }

    /// Record what the update channel says.
    pub fn set_update(&mut self, state: UpdateState) {
        self.machine.set_update(state);
    }

    /// Record the resources a repair found drifted.
    pub fn set_drift(&mut self, resources: Vec<String>) {
        self.machine.set_drift(resources);
    }

    /// Send the current state to the running child.
    pub fn publish(&self) -> Result<(), zup_preset_host::SessionError> {
        let Some(child) = self.child.as_ref() else {
            return Ok(());
        };
        child.publish(
            self.configuration.clone(),
            Box::new(self.machine.snapshot().clone()),
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
    ///
    /// The child goes because it is dropped, which ends its tree; the state
    /// directories go because nothing will read them again. A session that ended
    /// any other way would leave both behind, which is why this is the one place
    /// that ends both rather than two callers that each remember to.
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

/// Write an executable so that a reader never sees a half-written one.
///
/// A temporary beside it and a rename over it, which is the same durability the
/// rest of this repository asks for and the reason a preset that reads a
/// just-materialized asset reads a whole one. The temporary's name is unique per
/// call, because two writers racing on one name is a way for one of them to
/// rename a file the other is still writing.
///
/// The permissions are set on the temporary, before the rename, because a file
/// that is briefly present and not executable is a file a concurrent launch can
/// fail on, and because setting them after would leave the same window. A file
/// created with `File::create` is mode `0666` on Unix - readable and writable,
/// never runnable - so a preset staged this way could be written and then
/// refused by `execve` with `EACCES`. Windows has no execute bit, so this is the
/// one platform where the distinction does not exist; naming the executable in
/// the call is what makes it the writer's job rather than the caller's.
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
    make_executable(&temporary)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Give a file the owner's execute bit, keeping what it already had.
///
/// Unix only, and deliberately additive: a staged preset was created `0666`, and
/// the one thing it is missing is the bit that lets it run. Reading and writing
/// are left as they were rather than widened, because this file is a copy of a
/// preset and nothing about staging it grants anything else. `Owned` rather than
/// the whole mask, because `0777` would make every staged file world-writable -
/// another process on the machine editing the preset this host is about to run.
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o100);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
