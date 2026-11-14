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
#[cfg(test)]
mod publication_tests;
mod publish;
mod signing;
mod toolchain;

use std::path::{Path, PathBuf};

use zup_presentation::ProcessOutcome;

pub use crate::cli::{FrontendArg, OutputArg, ScopeArg, ScopePolicyArg};
pub use crate::failure::Reporter;
pub use crate::toolchain::{
    ToolchainCache, ToolchainResolver, ToolchainSource, missing_component_message,
};

pub const DEFAULT_MANIFEST: &str = "zup.toml";

pub const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn process_exit_code(error: &miette::Report) -> u8 {
    if error.downcast_ref::<crate::failure::Coded>().is_some() {
        ProcessOutcome::Configuration.code() as u8
    } else {
        ProcessOutcome::Failure.code() as u8
    }
}

pub fn run() -> miette::Result<()> {
    cli::dispatch(cli::parse())
}

pub fn command() -> clap::Command {
    cli::parser()
}

/// host*, never on the target being built: a Windows host staging a Linux
pub fn toolchain_state_root() -> miette::Result<PathBuf> {
    host_state_root().map_err(|error| failure::error("zup.toolchain.state_root", error))
}

fn host_state_root() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        zup_windows::default_state_root(
            &zup_core::AppId::new("dev.zup.toolchain").expect("toolchain application identity"),
            zup_core::SelectedScope::User,
        )
        .map_err(|error| error.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        zup_linux::state_root(
            &zup_core::AppId::new("dev.zup.toolchain").expect("toolchain application identity"),
            zup_core::SelectedScope::User,
        )
        .map_err(|error| error.to_string())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err("this build host has no zup state root".to_owned())
    }
}

pub fn resolver(toolchain_root: Option<PathBuf>) -> miette::Result<ToolchainResolver> {
    let executable = std::env::current_exe()
        .map_err(|error| failure::error("zup.toolchain.executable", error.to_string()))?;
    Ok(
        ToolchainResolver::new(ZUP_VERSION.to_owned(), executable, ToolchainCache::Host)
            .with_root(toolchain_root),
    )
}

pub fn plain_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        zup_windows::plain_path_text(path)
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests;
