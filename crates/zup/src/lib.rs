//! `zup`, the developer tool.
//!
//! This is the authoring and distribution surface: it reads a `zup.toml`, walks a
//! source tree, compiles plugins, composes distribution artifacts, stages a
//! release, generates a CI pipeline, and formats a manifest. It is a tool a person
//! installs once and runs from a project directory.
//!
//! It is not, and no longer contains, the application runtime. Installing,
//! modifying, repairing, updating and uninstalling an application happen in the
//! generated installer - a different package, with its own command surface, that
//! ships to end users and cannot see a manifest, a source tree, or a publisher.
//! Keeping those two things in one binary was why `cargo run` used to need a
//! feature flag to say `--help`: one executable was pretending to be two
//! products, and Cargo features were being asked to choose between them.

mod artifacts;
pub mod automation;
mod build;
mod build_inputs;
mod check;
pub mod ci;
mod cli;
pub mod doctor;
pub mod failure;
mod init;
mod inspect_artifact;
mod manifest_tools;
mod packages;
pub mod plugin;
pub mod preset;
pub mod preview;
pub mod project;
mod publish;
mod publish_github;
pub mod report;
mod signing;
mod toolchain;
pub mod toolchain_cli;

use std::path::{Path, PathBuf};

use zup_presentation::ProcessOutcome;

pub use crate::cli::{FrontendArg, OutputArg, ScopeArg};
pub use crate::report::Reporter;
pub use crate::toolchain::{ToolchainResolver, ToolchainSource, missing_component_message};

/// The manifest every authoring command defaults to.
pub const DEFAULT_MANIFEST: &str = "zup.toml";

/// This zup release, and the one a toolchain component must have come from.
pub const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The process exit code a failure reports.
pub fn process_exit_code(error: &miette::Report) -> u8 {
    ProcessOutcome::from_message(&error.to_string())
        .code()
        .clamp(1, 255) as u8
}

/// Run the developer CLI.
pub fn run() -> miette::Result<()> {
    cli::dispatch(cli::parse())
}

/// The developer CLI's parser, for the product-surface tests.
pub fn command() -> clap::Command {
    cli::parser()
}

/// The state root a developer's toolchain cache lives under.
///
/// A developer's machine has exactly one zup state root per scope, and the
/// toolchain cache belongs beside everything else zup keeps there rather than in
/// a directory of its own invention. Fallible rather than defaulted: a cache
/// search that silently fell back to the working directory would find a
/// developer's staged toolchain on a colleague's machine and nothing on a
/// clean one, which reads as a missing component rather than as a machine that
/// could not name its own directories.
pub fn toolchain_state_root() -> miette::Result<PathBuf> {
    zup_windows::default_state_root(zup_core::SelectedScope::User)
        .map_err(|error| failure::error("zup.toolchain.state_root", error.to_string()))
}

/// The resolver a build and a readiness report share.
pub fn resolver(toolchain_root: Option<PathBuf>) -> miette::Result<ToolchainResolver> {
    let executable = std::env::current_exe()
        .map_err(|error| failure::error("zup.toolchain.executable", error.to_string()))?;
    Ok(
        ToolchainResolver::new(ZUP_VERSION.to_owned(), executable, toolchain_state_root()?)
            .with_root(toolchain_root),
    )
}

/// A build-machine path, without the Windows verbatim prefix.
pub fn plain_path(path: &Path) -> String {
    zup_windows::plain_path_text(path)
}

#[cfg(test)]
mod tests;
