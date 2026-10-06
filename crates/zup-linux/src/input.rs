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
//!
//! Generated integration files (desktop entries, MIME packages) lower as
//! ordinary files. When such a file is created, replaced, repaired, or
//! removed, the derived database it feeds must be regenerated: a typed
//! refresh operation joins the transaction, ordered after the files by the
//! transaction graph.

use zup_core::Privilege;
use zup_exec::{ExecutionPlan, FileOperationKind, OwnedResource};
use zup_platform::TargetPlan;
use zup_transaction::{
    BackendOperation, FileDelta, FilePrecondition, FileRemoval, FileRemovalKind, FileWork,
    TransactionInput,
};

use crate::integration::GENERATED_PREFIX;
use crate::refresh::RefreshRequest;

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

    let mut mime_directory: Option<String> = None;
    let mut desktop_directory: Option<String> = None;
    let mut mime_after: Vec<zup_core::ResourceKey> = Vec::new();
    let mut desktop_after: Vec<zup_core::ResourceKey> = Vec::new();

    for file in &execution.files {
        let source = file.source_relative.as_str();
        if is_generated_source(source) && !matches!(file.kind, FileOperationKind::NoOp) {
            let (mime, desktop) = refresh_for_generated(source);
            if mime {
                mime_directory = mime_directory.or_else(|| {
                    file.destination
                        .parent()
                        .and_then(|packages| packages.parent())
                        .map(|directory| directory.to_string())
                });
            }
            if desktop {
                desktop_directory = desktop_directory.or_else(|| {
                    file.destination
                        .parent()
                        .map(|directory| directory.to_string())
                });
            }
        }
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
                let source = removal
                    .owned
                    .source_relative()
                    .map(|source| source.as_str())
                    .unwrap_or_default();
                if is_generated_source(source) {
                    let (mime, desktop) = refresh_for_generated(source);
                    if mime {
                        mime_directory = mime_directory.or_else(|| {
                            destination
                                .parent()
                                .and_then(|packages| packages.parent())
                                .map(|directory| directory.to_string())
                        });
                        mime_after.push(removal.key.clone());
                    }
                    if desktop {
                        desktop_directory = desktop_directory.or_else(|| {
                            destination.parent().map(|directory| directory.to_string())
                        });
                        desktop_after.push(removal.key.clone());
                    }
                }
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

    // The derived databases regenerate from the authoritative sources above:
    // one MIME refresh when a package source changed, one desktop refresh
    // when a desktop entry changed. A refresh that follows removals names
    // them as dependencies, so the database regenerates from the removed
    // world rather than from the sources about to be deleted. No other
    // operation kind reaches this backend, so these are the only refreshes a
    // Linux transaction can hold.
    if let Some(directory) = mime_directory {
        let request = RefreshRequest::mime(&directory);
        input.backend_operations.push(
            BackendOperation::apply(
                request.key(),
                request.backend_id(),
                Privilege::User,
                request.encode(),
            )
            .with_dependencies(mime_after),
        );
    }
    if let Some(directory) = desktop_directory {
        let request = RefreshRequest::desktop(&directory);
        input.backend_operations.push(
            BackendOperation::apply(
                request.key(),
                request.backend_id(),
                Privilege::User,
                request.encode(),
            )
            .with_dependencies(desktop_after),
        );
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

/// Whether a payload source is generated integration content rather than
/// package content.
fn is_generated_source(source: &str) -> bool {
    source == GENERATED_PREFIX || source.starts_with(&format!("{GENERATED_PREFIX}/"))
}

/// Which derived database a generated source feeds, by its stable prefix.
/// Unknown generated names refresh nothing: a future generated kind must opt
/// into its refresh explicitly rather than inherit one.
fn refresh_for_generated(source: &str) -> (bool, bool) {
    let mime = source.starts_with(&format!("{GENERATED_PREFIX}/mime/"));
    let desktop = source.starts_with(&format!("{GENERATED_PREFIX}/applications/"));
    (mime, desktop)
}
