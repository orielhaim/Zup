//! Where a preview session keeps the files it made.
//!
//! One directory per session, under the project, named by the caller. Everything
//! written here is disposable by construction: a run executable is a copy
//! replaced every time the window is replaced, and a materialized asset is
//! content-addressed beside nothing that outlives it. Nothing outside the
//! process ever reads any of it.
//!
//! The reason it is a type rather than two paths is that the watcher needs it.
//! Everything this process writes lives under one root, so excluding it from
//! filesystem events is one check rather than an enumeration that a later
//! version of this crate would have to remember to extend.

use std::path::{Path, PathBuf};

use zup_core::Sha256Digest;

/// The directory a project keeps a tool's own state in, beside the project.
pub const PROJECT_DIRECTORY: &str = ".zup";

/// How many generations of a replaced run executable are kept.
///
/// Bounded rather than complete, so a session left open for a day does not
/// accumulate two hundred copies of a twenty-megabyte binary. Two, because a
/// generation whose child is being started must not be deleted out from under
/// itself.
const KEPT_GENERATIONS: usize = 2;

/// One session's own directory under a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDirectory {
    root: PathBuf,
}

impl StateDirectory {
    /// The state directory for a session that calls itself `name`.
    pub fn under(project_root: &Path, name: &str) -> Self {
        Self {
            root: project_root.join(PROJECT_DIRECTORY).join(name),
        }
    }

    /// The state directory at an exact path, for a caller that chose one.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where one generation's run executable lives.
    ///
    /// A generation is one presented copy of the executable. It gets its own
    /// directory because the previous one is still running from the previous
    /// one, and on the platform this product targets a running executable cannot
    /// be overwritten. Sharing a path would mean the build had to wait for a
    /// person to close the window.
    pub fn run_executable(&self, generation: u64) -> PathBuf {
        self.root
            .join("runs")
            .join(generation.to_string())
            .join(format!(
                "preset{}",
                std::env::consts::EXE_SUFFIX.trim_start_matches('.')
            ))
    }

    /// Where this session materializes a file an application provided.
    ///
    /// Content-addressed for the same reason an installation's are: two settings
    /// that resolve to the same file are one file, and a file that changed is a
    /// new path, so a preset that already read the old one cannot be looking at
    /// stale content.
    pub fn asset_directory(&self, digest: Sha256Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.root.join("assets").join(&hex[..2]).join(&hex[2..])
    }

    /// Remove the run directories of generations that are no longer running.
    ///
    /// `live` is the generation a child is running from, which is never removed
    /// even when it is old: the file is open.
    pub fn retire(&self, live: Option<u64>) {
        let Ok(generations) = std::fs::read_dir(self.root.join("runs")) else {
            return;
        };
        let mut present: Vec<(u64, PathBuf)> = generations
            .flatten()
            .filter_map(|entry| {
                let number = entry.file_name().to_string_lossy().parse::<u64>().ok()?;
                Some((number, entry.path()))
            })
            .collect();
        present.sort_unstable_by_key(|(number, _)| std::cmp::Reverse(*number));
        for (number, path) in present.into_iter().skip(KEPT_GENERATIONS) {
            if live == Some(number) {
                continue;
            }
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
