//! The session: a simulated machine, a disposable child, and the loop that keeps
//! them in agreement.
//!
//! One process owns both, and the interesting part is what happens when one of
//! them has to give way. A selection that does not verify must not cost the
//! person their window; a selection that does verify must not replace the window
//! with nothing; a change that arrives while something is already happening must
//! not start a second one. All three are the same rule applied at different times:
//! the last thing known to work stays up until something better is demonstrably
//! running.
//!
//! The threads are here because the work is genuinely concurrent and blocking on
//! any of it would be wrong. A filesystem notification takes milliseconds, a
//! preset's window belongs to the person looking at it, and none of those should
//! wait for another.
//!
//! What is *not* here is what the world being previewed is. A driver supplies its
//! own change vocabulary and its own jobs; everything the child sees, everything
//! the controls do, and everything that happens when a child exits is decided
//! here, once, by the same code for every preview.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use zup_core::{Sha256Digest, UiPreset};
use zup_ui_host::HostDecision;
use zup_ui_protocol::{UiAction, UiSnapshot};

use crate::controls::{self, Command, Components, Effect};
use crate::simulator::{Scenario, Simulator, StageError};
use crate::state::StateDirectory;

/// How long the loop waits for something to happen before looking again.
///
/// Short enough that a child exiting is noticed while a person is still looking at
/// the window that closed, and long enough that an idle session costs nothing.
const TICK: Duration = Duration::from_millis(80);

/// A message from a thread to the session.
///
/// `Changed` and `Finished` carry the driver's own vocabulary, because what a
/// filesystem change means and what a job produces are facts about the world
/// being previewed. Everything else is a session's own.
#[derive(Debug)]
pub enum Event<C, F> {
    /// Something on disk changed, in the driver's reading of it.
    Changed(C),
    /// A job the driver started has finished.
    Finished(F),
    /// One line of input from the controls.
    Control(String),
    /// Print this.
    Report(String),
    /// The preview is finished.
    Stop,
}

impl<C, F> Event<C, F> {
    /// The channel a session's threads report into.
    pub fn channel() -> (Sender<Self>, Receiver<Self>) {
        mpsc::channel()
    }

    /// Read the control surface, and end the session when the terminal asks.
    ///
    /// A preset is a child this process launched, and a child outlives a parent
    /// terminated without running anything on the way out. Asking the session to
    /// stop means the child is ended and reaped; not asking leaves a window
    /// nobody is watching, holding its executable open.
    ///
    /// Takes the sender by value: both threads it starts outlive the call, and a
    /// caller that kept using its own handle afterwards would be reasoning about
    /// a channel it no longer fully owns.
    pub fn attach_terminal(sender: Sender<Self>) -> Result<(), String>
    where
        C: Send + 'static,
        F: Send + 'static,
    {
        let asking = sender.clone();
        ctrlc::set_handler(move || {
            let _ = asking.send(Self::Stop);
        })
        .map_err(|error| error.to_string())?;
        std::thread::Builder::new()
            .name("zup-preview-controls".into())
            .spawn(move || {
                use std::io::BufRead;
                let stdin = std::io::stdin();
                for line in stdin.lock().lines() {
                    let Ok(line) = line else { break };
                    if sender.send(Self::Control(line)).is_err() {
                        break;
                    }
                }
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// What a preview session's own layer does with the world changing under it.
///
/// Everything a caller has to say is here, and it is small on purpose: the state
/// machine, the session, the settings, the files and the child are not the
/// driver's to implement, and a driver that could reach them would be a second
/// implementation of the thing this crate exists to hold to one.
pub trait Driver {
    /// The driver's own reading of a filesystem change.
    type Change;
    /// The driver's own report that a job it started has finished.
    type Finished;

    /// The session this driver is presenting.
    ///
    /// Handed back rather than passed in, because a driver owns its session and
    /// also needs to reach its own state from inside a callback: passing both a
    /// session and a driver that holds one is a borrow two things cannot satisfy.
    fn runtime(&mut self) -> &mut Runtime;

    /// The session's own work for one turn, before anything is waited for.
    ///
    /// Two things happen here and nowhere else: a child that has exited is
    /// noticed, and the actions a child has asked for are fed to the state machine
    /// and published back. A caller driving the session by hand needs both, and
    /// giving it a second copy of this loop is how a driver and a session would
    /// come to disagree about when a preset's ask was honoured.
    fn pump(&mut self) {
        if !self.runtime().is_running() {
            self.runtime().notice_exit();
        }
        self.runtime().drain_actions();
    }

    /// Called once per turn, before anything is waited for.
    ///
    /// For work that must not overlap itself: a build that is already running
    /// makes a second one a cancellation race rather than a rebuild.
    fn tick(&mut self) {}

    /// Something on disk changed.
    fn changed(&mut self, change: Self::Change);

    /// A job the driver started has finished.
    fn finished(&mut self, outcome: Self::Finished);
}

/// Why a selection could not become a running child.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Staged(#[from] StageError),
    #[error(transparent)]
    Session(#[from] zup_ui_host::SessionError),
}

/// What a control did, in terms the session acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOutcome {
    /// Carry on.
    Handled,
    /// The preview is finished.
    Quit,
}

/// A preview session: the machine, the child, and the controls.
///
/// Everything a preset can observe passes through here, and nothing else can
/// reach the child. The child is presented the same state whether it was built by
/// Cargo, read out of a `.zupui`, or copied from anywhere else, because what it
/// sees is a [`Runtime`]'s configuration and a [`zup_ui_host::HostState`]'s
/// snapshot rather than anything about how it was chosen.
pub struct Runtime {
    simulator: Simulator,
    scenario: Scenario,
    /// Where the running child's actions arrive, replaced with the child.
    actions: Option<Receiver<UiAction>>,
}

impl Runtime {
    /// A session on a machine, with no child yet.
    pub fn new(state: StateDirectory, scenario: Scenario) -> Self {
        Self {
            simulator: Simulator::new(state, scenario.clone()),
            scenario,
            actions: None,
        }
    }

    /// The state a preset sees right now.
    pub fn snapshot(&self) -> &UiSnapshot {
        self.simulator.snapshot()
    }

    /// What the preset is told the application configured.
    pub fn configuration(&self) -> &zup_ui_protocol::UiConfiguration {
        self.simulator.configuration()
    }

    /// The machine, for a driver that needs to look at it.
    pub fn simulator(&self) -> &Simulator {
        &self.simulator
    }

    /// The machine, for a driver that has to change it.
    pub fn simulator_mut(&mut self) -> &mut Simulator {
        &mut self.simulator
    }

    /// The scenario a control reads and writes.
    pub fn scenario_mut(&mut self) -> &mut Scenario {
        &mut self.scenario
    }

    /// The generation currently running, if any.
    pub fn generation(&self) -> Option<u64> {
        self.simulator.generation()
    }

    /// Whether a child is alive.
    pub fn is_running(&mut self) -> bool {
        self.simulator.is_running()
    }

    /// One line of input.
    pub fn control(&mut self, line: &str) -> ControlOutcome {
        match Command::parse(line) {
            Err(error) if error.is_empty() => ControlOutcome::Handled,
            Err(error) => {
                println!("{error}\n\n{}", controls::COMMANDS);
                ControlOutcome::Handled
            }
            Ok(Command::Quit) => ControlOutcome::Quit,
            Ok(command) => {
                let effect = controls::apply(&mut self.simulator, &mut self.scenario, command);
                self.report(effect);
                ControlOutcome::Handled
            }
        }
    }

    /// A child that exited is noticed, not waited for.
    ///
    /// The session survives it: the state, the settings, and the selection that
    /// is still known to work are all still here.
    pub fn notice_exit(&mut self) {
        if self.simulator.generation().is_some() {
            println!("  stopped  the preset process exited; the session is still here");
            self.simulator.forget_child();
        }
    }

    /// Everything the running child has asked for since the last call.
    pub fn drain_actions(&mut self) {
        let Some(actions) = self.actions.as_ref() else {
            return;
        };
        let mut asked = Vec::new();
        while let Ok(action) = actions.try_recv() {
            asked.push(action);
        }
        for action in asked {
            self.asked(action);
        }
    }

    /// Show a verified selection, and only adopt it once it has opened the
    /// session.
    ///
    /// The generation is reported on success, because a person who replaced the
    /// window needs to know it was replaced rather than restarted. A failure
    /// leaves whatever was running exactly where it was: nothing above touches
    /// the previous child, and only a completed handshake swaps it.
    pub fn present(&mut self, preset: &UiPreset, bytes: &[u8]) -> Result<u64, StartError> {
        let candidate = self.simulator.stage(bytes, preset)?;
        let (asked, received) = mpsc::channel();
        self.simulator.adopt(candidate, asked)?;
        self.actions = Some(received);
        let generation = self
            .simulator
            .generation()
            .expect("an adopted child is a generation");
        println!("  running  generation {generation}");
        Ok(generation)
    }

    /// Replace the settings the preset is told, and send them.
    ///
    /// The caller has already proved them against a schema this runtime does not
    /// hold, and a configuration that does not fit leaves the last one in force
    /// because the caller kept it - which is the only place that answer can be
    /// given, since a preset that has not started cannot be asked.
    pub fn set_settings(&mut self, settings: serde_json::Value) {
        self.simulator.set_settings(settings);
        self.publish();
    }

    /// Materialize one application-provided file, and record where the preset
    /// will read it.
    pub fn set_asset(&mut self, name: &str, source: &Path) -> Result<Sha256Digest, String> {
        self.simulator.set_asset(name, source)
    }

    /// Forget every application-provided file.
    pub fn clear_assets(&mut self) {
        self.simulator.clear_assets();
    }

    /// Send the current state to the running child.
    pub fn publish(&mut self) {
        if let Err(error) = self.simulator.publish() {
            println!("  publish  {error}");
        }
    }

    /// End the session, and take the child with it.
    pub fn shutdown(&mut self) {
        self.simulator.shutdown();
    }

    /// A preset asked for something.
    fn asked(&mut self, action: UiAction) {
        match self.simulator.act(action) {
            HostDecision::Refused(refusal) => {
                println!("  refused  {refusal}");
            }
            // A lifecycle the engine would have run is the one thing a preview
            // cannot do on a person's behalf. The state is already running; what
            // is left is to say so.
            HostDecision::Run { .. } => {
                println!("  action   the engine is simulated; `next` steps it along");
            }
            _ => {}
        }
        self.publish();
    }

    fn report(&self, effect: Effect) {
        match effect {
            Effect::Published | Effect::Quit => {}
            Effect::Held => println!("  held     no preset is running to tell"),
            Effect::Refused(reason) => println!("  refused  {reason}"),
        }
    }
}

/// Run a preview until somebody quits it.
///
/// One loop, because the interesting cases are not the ones a driver
/// reimplements: a child that exits, a child that is replaced while an operation
/// is half done, an action that arrives between two changes. Each of those is a
/// decision about the *session*, and a session per command is a session per
/// command that will be different next year.
///
/// The loop does not end when a child does. The machine, the settings and the
/// files all outlive the window that was drawing them, and a session that ended
/// with a crashed preset would take the controls with it.
pub fn serve<D: Driver>(inbox: &Receiver<Event<D::Change, D::Finished>>, driver: &mut D) {
    // What the session prints is its product, and a redirected stdout is block
    // buffered by default, so a preview would say nothing at all while it works.
    let _lines = std::io::LineWriter::new(std::io::stdout());
    loop {
        driver.pump();
        driver.tick();
        match inbox.recv_timeout(TICK) {
            Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Ok(Event::Report(message)) => println!("{message}"),
            Ok(Event::Control(line)) => {
                if driver.runtime().control(&line) == ControlOutcome::Quit {
                    break;
                }
            }
            Ok(Event::Changed(change)) => driver.changed(change),
            Ok(Event::Finished(outcome)) => driver.finished(outcome),
        }
    }
    driver.runtime().shutdown();
}

/// The component layout a session opens with, when nothing else chose one.
///
/// One optional component beside a required one, because it is the smallest shape
/// that still shows both kinds and a session that opened with none would never
/// show a person that a component list can be empty.
pub fn default_scenario() -> Scenario {
    Scenario {
        components: Components::OneOptional.options(),
        ..Scenario::default()
    }
}
