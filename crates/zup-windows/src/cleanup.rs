use crate::filesystem::authority::{DirectoryAccess, FileAuthority, FileParent};
use zup_plan::{CleanupCandidate, DeploymentNode};
use zup_transaction::{CleanupCompletion, CleanupDeferral, OperationReceipt, TransactionRecord};

fn parent(
    operation: &zup_plan::CleanupOperation,
    file: &zup_plan::FixedCleanup,
) -> Result<(FileParent, std::ffi::OsString), String> {
    let path = crate::lowering::host_path(&file.path);
    let authority = match operation.privilege {
        zup_core::Privilege::User => FileAuthority::User(
            crate::transport::UserSid::current().map_err(|error| error.to_string())?,
        ),
        zup_core::Privilege::System => FileAuthority::Machine,
    };
    Ok((
        FileParent::open(
            path.parent().ok_or("retired file has no parent")?,
            authority,
            DirectoryAccess::CreateFiles,
        )
        .map_err(|error| error.to_string())?,
        path.file_name()
            .ok_or("retired file has no name")?
            .to_owned(),
    ))
}

pub(crate) fn apply(
    state: &crate::machine_state::StateAuthority,
    node: &DeploymentNode,
    transaction: &TransactionRecord,
) -> Result<OperationReceipt, String> {
    if state.app != transaction.app_id || state.scope != transaction.scope {
        return Err("cleanup authority differs from its authenticated application".into());
    }
    let root = &state.root;
    let installation = crate::ledger::InstallationRecordStore::new(root)
        .load(&transaction.app_id, transaction.scope)
        .map_err(|error| error.to_string())?
        .ok_or("cleanup installation record is missing")?;
    let store = if transaction.scope == zup_core::SelectedScope::Machine {
        let machine = state.machine().map_err(|error| error.to_string())?;
        if machine.path() != root {
            return Err("cleanup state root differs from machine authority".into());
        }
        machine.transaction_store()
    } else {
        zup_transaction::FilesystemTransactionStore::private_at(root)
    };
    let operation =
        zup_transaction::authenticate_cleanup(&store, &installation, transaction, node)?;
    let completion = match &operation.candidate {
        CleanupCandidate::History(history) => zup_transaction::HistoryStorage(
            crate::filesystem::history::WindowsHistoryFilesystem(state.clone()),
        )
        .collect(history)?,
        CleanupCandidate::Generation(generation) => zup_transaction::collect_generation(
            &crate::collection::WindowsCollector {
                state: state.clone(),
            },
            operation,
            generation,
            transaction.transaction_id,
        )?,
        CleanupCandidate::Fixed(file) => {
            let (parent, name) = parent(operation, file)?;
            if parent
                .file_absent(&name)
                .map_err(|error| error.to_string())?
            {
                CleanupCompletion::AlreadyAbsent
            } else {
                match parent.fence_owned_file(&name, file.instance.object, file.size, file.sha256) {
                    Ok(fence) => {
                        parent
                            .discard_fenced_file(fence)
                            .map_err(|error| error.to_string())?;
                        if !parent
                            .file_absent(&name)
                            .map_err(|error| error.to_string())?
                        {
                            return Err("retired file name is occupied after deletion".into());
                        }
                        CleanupCompletion::Removed
                    }
                    Err(error) if error.sharing_violation() => CleanupCompletion::Deferred {
                        reason: CleanupDeferral::InUse,
                    },
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    };
    Ok(OperationReceipt::Cleanup {
        intent: operation.fingerprint(),
        completion,
    })
}

pub(crate) fn verify(
    state: &crate::machine_state::StateAuthority,
    transaction: zup_core::TransactionId,
    node: &DeploymentNode,
    receipt: &OperationReceipt,
) -> Result<(), String> {
    let zup_plan::NodeKind::Cleanup { operation } = &node.kind else {
        return Err("cleanup effect required".into());
    };
    let OperationReceipt::Cleanup { intent, completion } = receipt else {
        return Err("cleanup receipt required".into());
    };
    if intent != &operation.fingerprint() {
        return Err("cleanup receipt differs from authenticated intent".into());
    }
    if let CleanupCandidate::History(history) = &operation.candidate {
        return zup_transaction::HistoryStorage(
            crate::filesystem::history::WindowsHistoryFilesystem(state.clone()),
        )
        .verify(history, *completion);
    }
    if let CleanupCandidate::Generation(generation) = &operation.candidate {
        return zup_transaction::verify_generation_collection(
            &crate::collection::WindowsCollector {
                state: state.clone(),
            },
            operation,
            generation,
            transaction,
            *completion,
        );
    }
    match completion {
        CleanupCompletion::Removed | CleanupCompletion::AlreadyAbsent => match &operation.candidate
        {
            CleanupCandidate::Fixed(file) => {
                let (parent, name) = parent(operation, file)?;
                if !parent
                    .file_absent(&name)
                    .map_err(|error| error.to_string())?
                {
                    return Err("collected file name is occupied".into());
                }
            }
            CleanupCandidate::Generation(_) | CleanupCandidate::History(_) => {
                return Err("generation collection has no native deletion evidence".into());
            }
        },
        CleanupCompletion::Deferred { .. } => {}
    }
    Ok(())
}
