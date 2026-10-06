//! Turning a portable execution plan into a transaction input.
//!
//! The mapping is mechanical - one file decision becomes one unit of
//! transaction work - and it is also a boundary. Everything this backend
//! cannot execute is refused here, before compilation, with the kind named:
//! a conflict the planner could not resolve, a drifted resource the ledger no
//! longer explains, or any non-file operation that reached this far despite
//! the capability gate. A transaction input with backend operations in it
//! would compile cleanly and then fail at apply time, which is exactly the
//! install-files-then-fail-on-services outcome the capability boundary exists
//! to prevent.

use zup_exec::{ExecutionPlan, FileOperationKind, OwnedResource};
use zup_platform::TargetPlan;
use zup_transaction::{
    FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork, TransactionInput,
};

/// Why an execution plan cannot become a Linux transaction input.
#[derive(Debug, thiserror::Error)]
pub enum LinuxInputError {
    #[error(
        "file `{destination}` is {kind:?}: the planner could not decide, so there is no transaction to run"
    )]
    Undecided {
        destination: String,
        kind: FileOperationKind,
    },

    #[error("cannot transact {kind}: no Linux mechanism executes it in this phase")]
    Unsupported { kind: &'static str },
}

/// Compile an execution plan into a transaction input for Linux.
///
/// Files and file removals lower directly; every other operation kind is
/// refused rather than dropped. Dropping one would install the files and
/// silently skip the rest, which reads as success and is not.
pub fn compile_execution_plan(
    execution: &ExecutionPlan,
    target: &TargetPlan,
) -> Result<TransactionInput, LinuxInputError> {
    let mut input = TransactionInput::new(target.target.clone());
    input.selected_components = execution.selected_components.clone();
    input.install_directory = Some(target.install_directory.clone());
    input.uninstall = execution.uninstall;
    // Console and headless installers present no window, so no preset runtime
    // travels with the transaction. A preset here would be bytes without a
    // presenter, which is content without a consumer.
    input.preset = None;

    for file in &execution.files {
        input.files.push(FileWork {
            key: file.key.clone(),
            source_relative: file.source_relative.clone(),
            destination: file.destination.clone(),
            precondition: match file.precondition {
                zup_exec::FilePrecondition::Absent => FilePrecondition::Absent,
                zup_exec::FilePrecondition::Exact { size, sha256 } => {
                    FilePrecondition::Exact { size, sha256 }
                }
            },
            expected_sha256: file.expected_sha256,
            expected_size: file.expected_size,
            privilege: file.privilege,
            executable: file.executable,
            delta: match file.kind {
                FileOperationKind::Create => FileDelta::Create,
                FileOperationKind::Replace => FileDelta::Replace,
                FileOperationKind::RestoreOwned => FileDelta::RestoreOwned,
                FileOperationKind::RepairOwned => FileDelta::RepairOwned,
                FileOperationKind::NoOp => FileDelta::NoOp,
                FileOperationKind::Conflict | FileOperationKind::Drift => {
                    return Err(LinuxInputError::Undecided {
                        destination: file.destination.to_string(),
                        kind: file.kind,
                    });
                }
            },
        });
    }

    for removal in &execution.removals {
        match &removal.owned {
            OwnedResource::File {
                destination,
                sha256,
                size,
                created_directories,
                ..
            } => {
                input.removals.push(FileRemoval {
                    key: removal.key.clone(),
                    kind: match removal.kind {
                        zup_exec::RemovalKind::RemoveOwned => FileRemovalKind::RemoveOwned,
                        zup_exec::RemovalKind::Drift => FileRemovalKind::Drift,
                    },
                    scope: removal.scope,
                    privilege: removal.privilege,
                    destination: destination.clone(),
                    sha256: *sha256,
                    size: *size,
                    created_directories: created_directories.clone(),
                });
                input.retired_keys.push(removal.key.clone());
            }
            _ => {
                return Err(LinuxInputError::Unsupported {
                    kind: "a non-file removal",
                });
            }
        }
    }

    for (kind, count) in [
        ("launcher", execution.launchers.len()),
        ("PATH entry", execution.path_entries.len()),
        ("service", execution.services.len()),
        ("URI protocol", execution.protocols.len()),
        ("file association", execution.file_associations.len()),
    ] {
        if count > 0 {
            return Err(LinuxInputError::Unsupported { kind });
        }
    }

    Ok(input)
}
