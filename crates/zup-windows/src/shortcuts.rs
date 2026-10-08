use std::fs;

use crate::transaction_payload::{BackendReceipt, NativeReconcileResult};
use zup_exec::{LauncherOperation, LauncherOperationKind, LauncherState, ObservedLauncherState};
use zup_platform::TargetPath;

use crate::durable::move_durable;
use crate::lowering::host_path;

#[derive(Debug, thiserror::Error)]
pub enum ShortcutError {
    #[error("shortcut `{path}` could not be read: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("shortcut `{path}` could not be written: {reason}")]
    Unwritable { path: String, reason: String },
    #[error("shortcut operation is not executable")]
    NotExecutable,
    #[error("invalid shortcut precondition")]
    InvalidPrecondition,
    #[error("shortcut `{0}` changed since planning")]
    ChangedSincePlanning(String),
    #[error("shortcut `{0}` changed after installation")]
    ChangedAfterInstall(String),
    #[error("shortcut has no parent")]
    MissingParent,
}

fn unreadable(path: &TargetPath, reason: impl std::fmt::Display) -> ShortcutError {
    ShortcutError::Unreadable {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

fn unwritable(path: &TargetPath, reason: impl std::fmt::Display) -> ShortcutError {
    ShortcutError::Unwritable {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

pub fn read_shortcut(launcher_path: &TargetPath) -> Result<ObservedLauncherState, ShortcutError> {
    let path = host_path(launcher_path);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservedLauncherState::Absent);
        }
        Err(err) => return Err(unreadable(launcher_path, err)),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(ObservedLauncherState::NonFile);
    }
    crate::shell_link::load_shortcut(&path, launcher_path.target())
        .map_err(|error| unreadable(launcher_path, error))
}

fn state(path: &TargetPath) -> Result<Option<LauncherState>, ShortcutError> {
    match read_shortcut(path)? {
        ObservedLauncherState::Absent => Ok(Some(LauncherState::Absent)),
        ObservedLauncherState::Launcher {
            target,
            arguments,
            working_directory,
        } => Ok(Some(LauncherState::Launcher {
            target,
            arguments,
            working_directory,
        })),
        ObservedLauncherState::InvalidLauncher | ObservedLauncherState::NonFile => Ok(None),
    }
}

fn previous(op: &LauncherOperation) -> Option<LauncherState> {
    match &op.previous {
        ObservedLauncherState::Absent => Some(LauncherState::Absent),
        ObservedLauncherState::Launcher {
            target,
            arguments,
            working_directory,
        } => Some(LauncherState::Launcher {
            target: target.clone(),
            arguments: arguments.clone(),
            working_directory: working_directory.clone(),
        }),
        _ => None,
    }
}

fn installed(op: &LauncherOperation) -> LauncherState {
    LauncherState::Launcher {
        target: op.target.clone(),
        arguments: op.arguments.clone(),
        working_directory: op.working_directory.clone(),
    }
}

fn write(
    launcher_path: &TargetPath,
    value: &LauncherState,
    icon: Option<&zup_platform::TargetPath>,
) -> Result<(), ShortcutError> {
    let host = host_path(launcher_path);
    match value {
        LauncherState::Absent => {
            std::fs::remove_file(&host).map_err(|error| unwritable(launcher_path, error))
        }
        LauncherState::Launcher {
            target,
            arguments,
            working_directory,
        } => {
            let parent = host.parent().ok_or(ShortcutError::MissingParent)?;
            std::fs::create_dir_all(parent).map_err(|error| unwritable(launcher_path, error))?;
            let temporary = host.with_extension(format!("zup-{}.lnk", uuid::Uuid::now_v7()));
            let result = (|| {
                crate::shell_link::save_shortcut(
                    &host,
                    &temporary,
                    target,
                    arguments,
                    working_directory.as_ref(),
                    icon,
                )
                .map_err(|error| unwritable(launcher_path, error))?;
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&temporary)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| unwritable(launcher_path, error))?;
                move_durable(&temporary, &host).map_err(|error| unwritable(launcher_path, error))
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        }
    }
}

pub fn apply(op: &LauncherOperation) -> Result<BackendReceipt, ShortcutError> {
    if !matches!(
        op.kind,
        LauncherOperationKind::Create
            | LauncherOperationKind::UpdateOwned
            | LauncherOperationKind::RestoreOwned
    ) {
        return Err(ShortcutError::NotExecutable);
    }
    let previous = previous(op).ok_or(ShortcutError::InvalidPrecondition)?;
    if state(&op.launcher_path)? != Some(previous.clone()) {
        return Err(ShortcutError::ChangedSincePlanning(
            op.launcher_path.to_string(),
        ));
    }
    let installed = installed(op);
    write(&op.launcher_path, &installed, op.icon.as_ref())?;
    Ok(BackendReceipt::Launcher {
        launcher_path: op.launcher_path.clone(),
        privilege: op.privilege,
        previous,
        installed,
    })
}

pub fn rollback(
    launcher_path: &TargetPath,
    previous: &LauncherState,
    installed: &LauncherState,
) -> Result<(), ShortcutError> {
    if state(launcher_path)? != Some(installed.clone()) {
        return Err(ShortcutError::ChangedAfterInstall(
            launcher_path.to_string(),
        ));
    }
    write(launcher_path, previous, None)
}

pub fn reconcile(op: &LauncherOperation) -> Result<NativeReconcileResult, ShortcutError> {
    let current = match state(&op.launcher_path) {
        Ok(Some(current)) => current,
        _ => return Ok(NativeReconcileResult::Ambiguous),
    };
    let Some(previous) = previous(op) else {
        return Ok(NativeReconcileResult::Ambiguous);
    };
    let installed = installed(op);
    if current == installed {
        Ok(NativeReconcileResult::AppliedWithReceipt(Box::new(
            BackendReceipt::Launcher {
                launcher_path: op.launcher_path.clone(),
                privilege: op.privilege,
                previous,
                installed,
            },
        )))
    } else if current == previous {
        Ok(NativeReconcileResult::NotApplied)
    } else {
        Ok(NativeReconcileResult::Ambiguous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lowering::target_path_from_host;
    use crate::transaction_payload::NativeReconcileResult;
    use tempfile::TempDir;
    use zup_core::{LauncherLocation, ResourceKey, TargetTriple};

    #[test]
    fn native_shortcut_create_update_and_ownership_safe_rollback() {
        let dir = TempDir::new().unwrap();
        let root = std::path::PathBuf::from(crate::machine_state::plain_path_text(
            &std::fs::canonicalize(dir.path()).unwrap(),
        ));
        let target_triple = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let launcher_path = target_path_from_host(&root.join("Acme.lnk"), &target_triple).unwrap();
        let target = target_path_from_host(&root.join("Acme App.exe"), &target_triple).unwrap();
        std::fs::write(host_path(&target), b"test").unwrap();
        let mut op = LauncherOperation {
            key: ResourceKey::Launcher {
                location: LauncherLocation::Desktop,
                name: "Acme".into(),
            },
            kind: LauncherOperationKind::Create,
            launcher_path: launcher_path.clone(),
            target: target.clone(),
            arguments: vec!["a b".into(), "quoted\"text".into(), "世界".into()],
            working_directory: Some(target_path_from_host(&root, &target_triple).unwrap()),
            privilege: zup_core::Privilege::User,
            previous: ObservedLauncherState::Absent,
            conflict: None,
            icon: None,
        };
        assert_eq!(reconcile(&op).unwrap(), NativeReconcileResult::NotApplied);
        let first = apply(&op).unwrap();
        assert_eq!(state(&launcher_path).unwrap(), Some(installed(&op)));
        assert!(matches!(
            reconcile(&op).unwrap(),
            NativeReconcileResult::AppliedWithReceipt(_)
        ));
        let mut decorated = lnks::Shortcut::load(host_path(&launcher_path)).unwrap();
        decorated.description = Some("user description".into());
        decorated.run_as_admin = true;
        decorated.save(host_path(&launcher_path)).unwrap();
        let BackendReceipt::Launcher {
            previous,
            installed: first_installed,
            ..
        } = &first
        else {
            panic!("shortcut receipt")
        };
        op.kind = LauncherOperationKind::UpdateOwned;
        op.previous = read_shortcut(&launcher_path).unwrap();
        op.arguments = vec!["new".into()];
        let upgraded = apply(&op).unwrap();
        assert_eq!(
            lnks::Shortcut::load(host_path(&launcher_path))
                .unwrap()
                .description
                .as_deref(),
            Some("user description")
        );
        assert!(
            lnks::Shortcut::load(host_path(&launcher_path))
                .unwrap()
                .run_as_admin
        );
        let BackendReceipt::Launcher {
            previous: old,
            installed: new,
            ..
        } = &upgraded
        else {
            panic!("shortcut receipt")
        };
        assert_eq!(old, first_installed);
        rollback(&launcher_path, old, new).unwrap();
        assert_eq!(
            lnks::Shortcut::load(host_path(&launcher_path))
                .unwrap()
                .description
                .as_deref(),
            Some("user description")
        );
        assert!(
            lnks::Shortcut::load(host_path(&launcher_path))
                .unwrap()
                .run_as_admin
        );
        assert_eq!(
            state(&launcher_path).unwrap(),
            Some(first_installed.clone())
        );
        let foreign = LauncherState::Launcher {
            target,
            arguments: vec!["foreign".into()],
            working_directory: None,
        };
        write(&launcher_path, &foreign, None).unwrap();
        assert!(rollback(&launcher_path, previous, first_installed).is_err());
        assert_eq!(reconcile(&op).unwrap(), NativeReconcileResult::Ambiguous);
    }
}
