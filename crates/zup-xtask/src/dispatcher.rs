//! The dispatcher: a build input the artifact tests require, and the rule that
//! says which machine it must be.
//!
//! The dispatcher is its own package with a deliberately small dependency
//! closure, so `cargo test` never builds it. That is the right trade for a
//! package whose size is a design constraint, and the wrong trade for the
//! composition tests, which have nothing to compose into without one. So it is
//! built as its own step — `scripts/build-dispatcher.ps1`, which CI runs before
//! the tests — and a test that needs one says so instead of quietly passing
//! without testing anything.
//!
//! Everything here is host-neutral path arithmetic. Which machine the
//! dispatcher targets is a Windows decision, and it belongs to the crate that
//! makes it.

use std::path::{Path, PathBuf};

/// The package that produces the dispatcher.
pub const PACKAGE: &str = "zup-dispatch";

/// The two dispatcher images: a windowed launcher and a console one.
pub const GUI: &str = "zup-dispatch";
pub const CONSOLE: &str = "zup-dispatch-console";

/// Why a dispatcher template is not where it should be.
#[derive(Debug, thiserror::Error)]
#[error(
    "the {name} dispatcher is not in {directory}; run scripts/build-dispatcher.ps1 and try again"
)]
pub struct Missing {
    pub name: String,
    pub directory: String,
}

/// One dispatcher image in `directory`.
pub fn beside(directory: &Path, name: &str) -> Result<PathBuf, Missing> {
    let candidate = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(Missing {
            name: name.to_owned(),
            directory: directory.display().to_string(),
        })
    }
}

/// The directory beside a running test executable, which is where a build step
/// installs the images the tests compose into.
pub fn beside_test_executable(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    let mut directory = executable.parent().unwrap_or(executable).to_path_buf();
    if directory.ends_with("deps") {
        directory.pop();
    }
    beside(&directory, name)
}
