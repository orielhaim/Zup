//! Deciding what a change to a preset project means.
//!
//! Three kinds of change, and treating them as one is the difference between a
//! development environment that feels instant and one that recompiles a whole
//! GPUI dependency tree because a logo was re-saved:
//!
//! ```text
//! a source file       the preset's code changed; rebuild and replace the child
//! the document        the application changed what it configures; resend it
//! an asset            a file the application provides changed; rematerialize it
//! ```
//!
//! The distinction is the whole reason a settings change feels immediate. A
//! rebuilt binary is the slowest thing this tool does, and spending it on a
//! document is spending minutes on a comma.
//!
//! The classification is here rather than in the shared runtime because the answer
//! belongs to the world being watched. An application project has the same shapes
//! and answers differently: a save to a `.rs` file is nothing at all there, and a
//! change to a `.zupui` is a replacement. Deciding that once, in the shared
//! crate, would be deciding it for both.

use std::collections::BTreeSet;
use std::path::PathBuf;

use zup_preview::{Seen, StateDirectory, Watcher as Debounced};

use crate::development::{Development, FILE_NAME};
use crate::project::Project;

/// What changed, and therefore what is worth doing about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The preset's own code, its manifest, or a build script.
    Source,
    /// The document describing the application.
    Configuration,
    /// One of the files that document provides.
    Asset(PathBuf),
}

impl Change {
    /// Whether this change needs a compiler.
    pub fn needs_build(&self) -> bool {
        matches!(self, Self::Source)
    }
}

/// What one round of watching produced, in this project's reading of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    /// Something changed, and which kind of change it is.
    Changed(Change),
    /// The backend reported a problem, and may or may not keep reporting.
    Failed(String),
}

/// Watches a preset project and says what changed.
pub struct Watcher {
    project: Project,
    development: Development,
    inner: Debounced,
}

impl Watcher {
    /// Start watching `project`, and classify what its events mean.
    pub fn start(project: &Project, development: &Development) -> Result<Self, String> {
        let inner = Debounced::start(&project.root, StateDirectory::under(&project.root, "dev"))?;
        Ok(Self {
            project: project.clone(),
            development: development.clone(),
            inner,
        })
    }

    /// The next thing the backend saw, or `None` when the watcher stops.
    ///
    /// A backend failure is reported rather than dropped, because a session that
    /// has stopped watching is indistinguishable from one in which nothing needed
    /// rebuilding, and nothing would explain the difference.
    #[allow(
        clippy::should_implement_trait,
        reason = "a watcher reports what changed, not items"
    )]
    pub fn next_change(&mut self) -> Option<Watched> {
        loop {
            let seen = self.inner.next_change()?;
            let paths = match seen {
                Seen::Changed(paths) => paths,
                Seen::Failed(reason) => return Some(Watched::Failed(reason)),
            };
            if let Some(change) = self.classify(paths) {
                return Some(Watched::Changed(change));
            }
        }
    }

    /// The first change worth acting on among `paths`, in the order that costs the
    /// least to do.
    ///
    /// The cheap half first, because a save that touched both a document and an
    /// asset should do both, and one that touched source has to rebuild whatever
    /// else it also did.
    fn classify(&self, paths: BTreeSet<PathBuf>) -> Option<Change> {
        // Both sides are derived from `self.project.root`, which is canonical, so
        // they are the same spelling of the same file. The document is recognised
        // by its own name rather than by a path, because it is the one file whose
        // location is a fact about the project rather than about a document's
        // contents.
        let assets = self.development.watched_files(&self.project.root);
        let mut configuration = false;
        let mut changed_asset = None;
        let mut source = false;
        for path in paths {
            if path.file_name().is_some_and(|name| name == FILE_NAME) {
                configuration = true;
            } else if assets.iter().any(|file| file == &path) {
                changed_asset = Some(path);
            } else {
                source = true;
            }
        }
        if let Some(asset) = changed_asset {
            return Some(Change::Asset(asset));
        }
        if configuration {
            return Some(Change::Configuration);
        }
        source.then_some(Change::Source)
    }
}
