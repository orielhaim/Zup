//! Shell Link (`.lnk`) inspection and managed mutation.

use std::fs;

use zup_exec::{ObservedShortcutState, ShortcutOperation, ShortcutOperationKind, ShortcutState};
use zup_platform::TargetPath;
use zup_transaction::{OperationReceipt, ReconcileResult};

use crate::durable::move_durable;

/// Read-only shortcut inspection surface.
pub trait ShortcutReader {
    fn read_shortcut(&self, link_path: &TargetPath) -> Result<ObservedShortcutState, String>;
}

/// Production COM Shell Link reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsShortcutReader;

impl ShortcutReader for WindowsShortcutReader {
    fn read_shortcut(&self, link_path: &TargetPath) -> Result<ObservedShortcutState, String> {
        let path = link_path.as_path();
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ObservedShortcutState::Absent);
            }
            Err(err) => return Err(err.to_string()),
        };
        let ft = meta.file_type();
        if ft.is_symlink() || !ft.is_file() {
            return Ok(ObservedShortcutState::NonFile);
        }
        crate::shell_link::load_shortcut(path)
    }
}

/// In-memory shortcut table for tests.
#[derive(Debug, Default, Clone)]
pub struct FakeShortcutReader {
    pub shortcuts: std::collections::BTreeMap<String, ObservedShortcutState>,
}

impl ShortcutReader for FakeShortcutReader {
    fn read_shortcut(&self, link_path: &TargetPath) -> Result<ObservedShortcutState, String> {
        Ok(self
            .shortcuts
            .get(&link_path.to_string())
            .cloned()
            .unwrap_or(ObservedShortcutState::Absent))
    }
}

fn state(path: &TargetPath) -> Result<Option<ShortcutState>, String> {
    match WindowsShortcutReader.read_shortcut(path)? {
        ObservedShortcutState::Absent => Ok(Some(ShortcutState::Absent)),
        ObservedShortcutState::Shortcut {
            target,
            arguments,
            working_directory,
        } => Ok(Some(ShortcutState::Link {
            target,
            arguments,
            working_directory,
        })),
        ObservedShortcutState::InvalidShortcut | ObservedShortcutState::NonFile => Ok(None),
    }
}

fn previous(op: &ShortcutOperation) -> Option<ShortcutState> {
    match &op.previous {
        ObservedShortcutState::Absent => Some(ShortcutState::Absent),
        ObservedShortcutState::Shortcut {
            target,
            arguments,
            working_directory,
        } => Some(ShortcutState::Link {
            target: target.clone(),
            arguments: arguments.clone(),
            working_directory: working_directory.clone(),
        }),
        _ => None,
    }
}

fn installed(op: &ShortcutOperation) -> ShortcutState {
    ShortcutState::Link {
        target: op.target.clone(),
        arguments: op.arguments.clone(),
        working_directory: op.working_directory.clone(),
    }
}

fn write(path: &TargetPath, value: &ShortcutState) -> Result<(), String> {
    match value {
        ShortcutState::Absent => std::fs::remove_file(path.as_path()).map_err(|e| e.to_string()),
        ShortcutState::Link {
            target,
            arguments,
            working_directory,
        } => {
            let parent = path.as_path().parent().ok_or("shortcut has no parent")?;
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            let temporary = path
                .as_path()
                .with_extension(format!("zup-{}.lnk", uuid::Uuid::now_v7()));
            let result = (|| {
                crate::shell_link::save_shortcut(
                    path.as_path(),
                    &temporary,
                    target,
                    arguments,
                    working_directory.as_ref(),
                )?;
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&temporary)
                    .and_then(|file| file.sync_all())
                    .map_err(|e| e.to_string())?;
                move_durable(&temporary, path.as_path()).map_err(|e| e.to_string())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        }
    }
}

pub fn apply(op: &ShortcutOperation) -> Result<OperationReceipt, String> {
    if !matches!(
        op.kind,
        ShortcutOperationKind::Create
            | ShortcutOperationKind::UpdateOwned
            | ShortcutOperationKind::RestoreOwned
    ) {
        return Err("shortcut is not executable".into());
    }
    let previous = previous(op).ok_or("invalid shortcut precondition")?;
    if state(&op.link_path)? != Some(previous.clone()) {
        return Err("shortcut changed since planning".into());
    }
    let installed = installed(op);
    write(&op.link_path, &installed)?;
    Ok(OperationReceipt::Shortcut {
        link_path: op.link_path.clone(),
        previous: Box::new(previous),
        installed: Box::new(installed),
    })
}

pub fn rollback(
    link_path: &TargetPath,
    previous: &ShortcutState,
    installed: &ShortcutState,
) -> Result<(), String> {
    if state(link_path)? != Some(installed.clone()) {
        return Err("shortcut changed after installation".into());
    }
    write(link_path, previous)
}

pub fn reconcile(op: &ShortcutOperation) -> Result<ReconcileResult, String> {
    let current = match state(&op.link_path) {
        Ok(Some(current)) => current,
        _ => return Ok(ReconcileResult::Ambiguous),
    };
    let Some(previous) = previous(op) else {
        return Ok(ReconcileResult::Ambiguous);
    };
    let installed = installed(op);
    if current == installed {
        Ok(ReconcileResult::AppliedWithReceipt(
            OperationReceipt::Shortcut {
                link_path: op.link_path.clone(),
                previous: Box::new(previous),
                installed: Box::new(installed),
            },
        ))
    } else if current == previous {
        Ok(ReconcileResult::NotApplied)
    } else {
        Ok(ReconcileResult::Ambiguous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zup_core::{ResourceKey, ShortcutLocation};

    #[test]
    fn native_shortcut_create_update_and_ownership_safe_rollback() {
        let dir = TempDir::new().unwrap();
        let link_path = TargetPath::new(dir.path().join("Acme.lnk")).unwrap();
        let target = TargetPath::new(dir.path().join("Acme App.exe")).unwrap();
        std::fs::write(target.as_path(), b"test").unwrap();
        let mut op = ShortcutOperation {
            key: ResourceKey::Shortcut {
                location: ShortcutLocation::Desktop,
                name: "Acme".into(),
            },
            kind: ShortcutOperationKind::Create,
            link_path: link_path.clone(),
            target: target.clone(),
            arguments: vec!["a b".into(), "quoted\"text".into(), "世界".into()],
            working_directory: Some(TargetPath::new(dir.path().to_path_buf()).unwrap()),
            previous: ObservedShortcutState::Absent,
            conflict: None,
        };
        assert_eq!(reconcile(&op).unwrap(), ReconcileResult::NotApplied);
        let first = apply(&op).unwrap();
        assert_eq!(state(&link_path).unwrap(), Some(installed(&op)));
        assert!(matches!(
            reconcile(&op).unwrap(),
            ReconcileResult::AppliedWithReceipt(_)
        ));
        let mut decorated = lnks::Shortcut::load(link_path.as_path()).unwrap();
        decorated.description = Some("user description".into());
        decorated.run_as_admin = true;
        decorated.save(link_path.as_path()).unwrap();
        let OperationReceipt::Shortcut {
            previous,
            installed: first_installed,
            ..
        } = &first
        else {
            panic!("shortcut receipt")
        };
        op.kind = ShortcutOperationKind::UpdateOwned;
        op.previous = WindowsShortcutReader.read_shortcut(&link_path).unwrap();
        op.arguments = vec!["new".into()];
        let upgraded = apply(&op).unwrap();
        assert_eq!(
            lnks::Shortcut::load(link_path.as_path())
                .unwrap()
                .description
                .as_deref(),
            Some("user description")
        );
        assert!(
            lnks::Shortcut::load(link_path.as_path())
                .unwrap()
                .run_as_admin
        );
        let OperationReceipt::Shortcut {
            previous: old,
            installed: new,
            ..
        } = &upgraded
        else {
            panic!("shortcut receipt")
        };
        assert_eq!(old, first_installed);
        rollback(&link_path, old, new).unwrap();
        assert_eq!(
            lnks::Shortcut::load(link_path.as_path())
                .unwrap()
                .description
                .as_deref(),
            Some("user description")
        );
        assert!(
            lnks::Shortcut::load(link_path.as_path())
                .unwrap()
                .run_as_admin
        );
        assert_eq!(state(&link_path).unwrap(), Some(*first_installed.clone()));
        let foreign = ShortcutState::Link {
            target,
            arguments: vec!["foreign".into()],
            working_directory: None,
        };
        write(&link_path, &foreign).unwrap();
        assert!(rollback(&link_path, previous, first_installed).is_err());
        assert_eq!(reconcile(&op).unwrap(), ReconcileResult::Ambiguous);
    }
}
