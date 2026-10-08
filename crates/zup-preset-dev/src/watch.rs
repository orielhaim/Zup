use std::collections::BTreeSet;
use std::path::PathBuf;

use zup_preview::{Seen, StateDirectory, Watcher as Debounced};

use crate::development::{Development, FILE_NAME};
use crate::project::Project;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Source,
    Configuration,
    Asset(PathBuf),
}

impl Change {
    pub fn needs_build(&self) -> bool {
        matches!(self, Self::Source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    Changed(Change),
    Failed(String),
}

pub struct Watcher {
    project: Project,
    development: Development,
    inner: Debounced,
}

impl Watcher {
    pub fn start(project: &Project, development: &Development) -> Result<Self, String> {
        let inner = Debounced::start(&project.root, StateDirectory::under(&project.root, "dev"))?;
        Ok(Self {
            project: project.clone(),
            development: development.clone(),
            inner,
        })
    }

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

    fn classify(&self, paths: BTreeSet<PathBuf>) -> Option<Change> {
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
