//! Shell Link (`.lnk`) inspection and managed mutation.

use std::fs;

use crate::transaction_payload::{BackendReceipt, NativeReconcileResult};
use zup_exec::{LauncherOperation, LauncherOperationKind, LauncherState, ObservedLauncherState};
use zup_platform::TargetPath;

use crate::durable::move_durable;
use crate::lowering::host_path;

/// Read-only shortcut inspection surface.
pub trait ShortcutReader {
    fn read_shortcut(&self, launcher_path: &TargetPath) -> Result<ObservedLauncherState, String>;
}

/// Production COM Shell Link reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsShortcutReader;

impl ShortcutReader for WindowsShortcutReader {
    fn read_shortcut(&self, launcher_path: &TargetPath) -> Result<ObservedLauncherState, String> {
        let path = host_path(launcher_path);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ObservedLauncherState::Absent);
            }
            Err(err) => return Err(err.to_string()),
        };
        let ft = meta.file_type();
        if ft.is_symlink() || !ft.is_file() {
            return Ok(ObservedLauncherState::NonFile);
        }
        crate::shell_link::load_shortcut(&path, launcher_path.target())
    }
}

/// In-memory shortcut table for tests.
#[derive(Debug, Default, Clone)]
pub struct FakeShortcutReader {
    pub shortcuts: std::collections::BTreeMap<String, ObservedLauncherState>,
}

impl ShortcutReader for FakeShortcutReader {
    fn read_shortcut(&self, launcher_path: &TargetPath) -> Result<ObservedLauncherState, String> {
        Ok(self
            .shortcuts
            .get(&launcher_path.to_string())
            .cloned()
            .unwrap_or(ObservedLauncherState::Absent))
    }
}

fn state(path: &TargetPath) -> Result<Option<LauncherState>, String> {
    match WindowsShortcutReader.read_shortcut(path)? {
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
    path: &TargetPath,
    value: &LauncherState,
    icon: Option<&zup_platform::TargetPath>,
) -> Result<(), String> {
    let path = host_path(path);
    match value {
        LauncherState::Absent => std::fs::remove_file(&path).map_err(|e| e.to_string()),
        LauncherState::Launcher {
            target,
            arguments,
            working_directory,
        } => {
            let parent = path.parent().ok_or("shortcut has no parent")?;
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            let temporary = path.with_extension(format!("zup-{}.lnk", uuid::Uuid::now_v7()));
            let result = (|| {
                crate::shell_link::save_shortcut(
                    &path,
                    &temporary,
                    target,
                    arguments,
                    working_directory.as_ref(),
                    icon,
                )?;
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&temporary)
                    .and_then(|file| file.sync_all())
                    .map_err(|e| e.to_string())?;
                move_durable(&temporary, &path).map_err(|e| e.to_string())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        }
    }
}

pub fn apply(op: &LauncherOperation) -> Result<BackendReceipt, String> {
    if !matches!(
        op.kind,
        LauncherOperationKind::Create
            | LauncherOperationKind::UpdateOwned
            | LauncherOperationKind::RestoreOwned
    ) {
        return Err("shortcut is not executable".into());
    }
    let previous = previous(op).ok_or("invalid shortcut precondition")?;
    if state(&op.launcher_path)? != Some(previous.clone()) {
        return Err("shortcut changed since planning".into());
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
) -> Result<(), String> {
    if state(launcher_path)? != Some(installed.clone()) {
        return Err("shortcut changed after installation".into());
    }
    write(launcher_path, previous, None)
}

pub fn reconcile(op: &LauncherOperation) -> Result<NativeReconcileResult, String> {
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
        // The resolved directory, not the one `TEMP` named. GitHub's Windows
        // runners hand out the 8.3 short form of the user's temp directory, and a
        // shortcut's target comes back the way the shell resolved it - so an
        // expectation built from the short name was comparing two spellings of one
        // file, and failing on every runner that hands one out. Resolving once here
        // keeps the strong claim: the shortcut holds exactly what was written.
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
        op.previous = WindowsShortcutReader.read_shortcut(&launcher_path).unwrap();
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
