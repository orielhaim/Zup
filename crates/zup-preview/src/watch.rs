//! Watching a project, and saying which of its changes matter.
//!
//! What is shared here is the mechanism and nothing else: the debouncer, the
//! recursive watch, the exclusion of a session's own directory, and the refusal
//! to swallow a backend error. What a change *means* belongs to whoever is
//! watching, because a preset project and an application project have different
//! answers: a save to a `.rs` file is a compile in one and nothing at all in the
//! other.
//!
//! The backend error is carried rather than discarded, because a session that has
//! quietly stopped watching is the worst outcome available: the author saves a
//! file and nothing happens, with nothing to explain it. A backend knows when it
//! has run out of watches, and that is worth more than the events it would
//! otherwise have produced.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

use crate::state::StateDirectory;

/// How long an editor's burst is collapsed over.
///
/// One save produces a write, a rename over, and a settle. Without a window over
/// them one save would start three things and cancel two.
const SETTLE: Duration = Duration::from_millis(120);

/// What one round of watching produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// These files changed, and nothing this session wrote is among them.
    Changed(BTreeSet<PathBuf>),
    /// The backend reported a problem, and may or may not keep reporting.
    Failed(String),
}

impl Seen {
    /// Whether this round produced a usable answer.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// A directory watcher that coalesces an editor's burst and reports failures.
pub struct Watcher {
    state: StateDirectory,
    events: Receiver<DebounceEventResult>,
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
}

impl Watcher {
    /// Start watching `root` recursively, ignoring what a session at `state` writes.
    pub fn start(root: &std::path::Path, state: StateDirectory) -> Result<Self, String> {
        let (sender, events) = mpsc::channel();
        // The sender is the handler rather than something wrapped around one:
        // `DebounceEventHandler` is implemented for it, so the debouncer already
        // has everything it needs, and a closure in between would only be a place
        // for a second answer to what an event is.
        let mut debouncer = new_debouncer(SETTLE, sender).map_err(|error| error.to_string())?;
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .map_err(|error| format!("{}: {error}", root.display()))?;
        Ok(Self {
            state,
            events,
            _debouncer: debouncer,
        })
    }

    /// The next thing the backend saw, or `None` when the watcher stops.
    ///
    /// Blocks, and is called on a thread of its own. A path this session wrote is
    /// dropped rather than reported, because a session that reacts to its own
    /// output would replace the child it just started with another one.
    #[allow(
        clippy::should_implement_trait,
        reason = "a watcher reports what changed, not items"
    )]
    pub fn next_change(&mut self) -> Option<Seen> {
        loop {
            let events = self.events.recv().ok()?;
            let paths: BTreeSet<PathBuf> = match events {
                Ok(events) => events
                    .into_iter()
                    .map(|event| event.path)
                    .filter(|path| !self.wrote(path))
                    .collect(),
                Err(error) => return Some(Seen::Failed(error.to_string())),
            };
            if !paths.is_empty() {
                return Some(Seen::Changed(paths));
            }
        }
    }

    /// Whether a path is one this session wrote.
    ///
    /// The whole project state directory rather than this session's own corner of
    /// it, because a session's directory is created one level at a time and the
    /// backend reports the level it created - so excluding only the session's own
    /// path would let the first write of the first session be reported back as
    /// somebody else's change. Cargo's `target` is excluded for the same reason in
    /// a different direction: a session that watched a preset project's build
    /// output would rebuild in a loop that never ends.
    fn wrote(&self, path: &std::path::Path) -> bool {
        path.starts_with(self.state.root())
            || path.components().any(|component| {
                let name = component.as_os_str();
                name == crate::state::PROJECT_DIRECTORY || name == "target"
            })
    }
}
