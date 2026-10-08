use std::path::Path;

use zup_core::TargetTriple;
use zup_exec::ObservedLauncherState;
use zup_platform::TargetPath;

use crate::cmdline::{quote_arg, split_command_line};
use crate::lowering::{host_path, target_path_from_host};

#[derive(Debug, thiserror::Error)]
pub enum ShellLinkError {
    #[error("shortcut `{path}` could not be read: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("shortcut `{path}` could not be written: {reason}")]
    Unwritable { path: String, reason: String },
}

fn unreadable(path: &Path, reason: impl std::fmt::Display) -> ShellLinkError {
    ShellLinkError::Unreadable {
        path: path.display().to_string(),
        reason: reason.to_string(),
    }
}

pub fn load_shortcut(
    path: &Path,
    target_triple: &TargetTriple,
) -> Result<ObservedLauncherState, ShellLinkError> {
    let link = match lnks::Shortcut::load(path) {
        Ok(link) => link,
        Err(lnks::Error::Io(error)) => return Err(unreadable(path, error)),
        Err(_) => return Ok(ObservedLauncherState::InvalidLauncher),
    };
    let Some(target) = link
        .target_path
        .as_deref()
        .and_then(|path| target_path_from_host(path, target_triple).ok())
    else {
        return Ok(ObservedLauncherState::InvalidLauncher);
    };
    let working_directory = match link.working_dir {
        Some(path) => match target_path_from_host(&path, target_triple) {
            Ok(path) => Some(path),
            Err(_) => return Ok(ObservedLauncherState::InvalidLauncher),
        },
        None => None,
    };
    Ok(ObservedLauncherState::Launcher {
        target,
        arguments: link
            .arguments
            .map_or_else(Vec::new, |line| split_command_line(&line)),
        working_directory,
    })
}

pub fn save_shortcut(
    current_path: &Path,
    output_path: &Path,
    target: &TargetPath,
    arguments: &[String],
    working_directory: Option<&TargetPath>,
    icon: Option<&TargetPath>,
) -> Result<(), ShellLinkError> {
    let mut link = if current_path.exists() {
        lnks::Shortcut::load(current_path).map_err(|error| ShellLinkError::Unwritable {
            path: output_path.display().to_string(),
            reason: error.to_string(),
        })?
    } else {
        lnks::Shortcut::new(host_path(target))
    };
    link.target_path = Some(host_path(target));
    link.arguments = if arguments.is_empty() {
        None
    } else {
        Some(
            arguments
                .iter()
                .map(|arg| quote_arg(arg))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    link.working_dir = working_directory.map(host_path);
    if let Some(icon) = icon {
        link.icon = Some(lnks::Icon::new(host_path(icon)));
    }
    link.save(output_path)
        .map_err(|error| ShellLinkError::Unwritable {
            path: output_path.display().to_string(),
            reason: error.to_string(),
        })
}
