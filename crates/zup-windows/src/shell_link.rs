//! Semantic Shell Link inspection and creation.

use std::path::Path;

use zup_exec::ObservedShortcutState;
use zup_platform::TargetPath;

use crate::cmdline::{quote_arg, split_command_line};

pub fn load_shortcut(path: &Path) -> Result<ObservedShortcutState, String> {
    let link = match lnks::Shortcut::load(path) {
        Ok(link) => link,
        Err(lnks::Error::Io(error)) => return Err(error.to_string()),
        Err(_) => return Ok(ObservedShortcutState::InvalidShortcut),
    };
    let Some(target) = link.target_path.and_then(|path| TargetPath::new(path).ok()) else {
        return Ok(ObservedShortcutState::InvalidShortcut);
    };
    let working_directory = match link.working_dir {
        Some(path) => match TargetPath::new(path) {
            Ok(path) => Some(path),
            Err(_) => return Ok(ObservedShortcutState::InvalidShortcut),
        },
        None => None,
    };
    Ok(ObservedShortcutState::Shortcut {
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
) -> Result<(), String> {
    let mut link = if current_path.exists() {
        lnks::Shortcut::load(current_path).map_err(|error| error.to_string())?
    } else {
        lnks::Shortcut::new(target.as_path())
    };
    link.target_path = Some(target.as_path().to_path_buf());
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
    link.working_dir = working_directory.map(|path| path.as_path().to_path_buf());
    link.save(output_path).map_err(|error| error.to_string())
}
