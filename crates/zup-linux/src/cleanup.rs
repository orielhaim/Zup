use crate::error::ExecError;
use zup_plan::{CleanupCandidate, DeploymentNode};
use zup_transaction::{CleanupCompletion, CleanupDeferral, OperationReceipt, TransactionRecord};

pub(crate) fn apply(
    state: &crate::state::StateAuthority,
    node: &DeploymentNode,
    transaction: &TransactionRecord,
) -> Result<OperationReceipt, ExecError> {
    state.validate(transaction)?;
    let installation = crate::ledger::LinuxInstallationStore::new(&state.root)
        .load(&transaction.app_id, transaction.scope)?
        .ok_or_else(|| {
            ExecError::refused(node.id.as_str(), "cleanup installation record is missing")
        })?;
    let store = zup_transaction::FilesystemTransactionStore::private_at(&state.root);
    let operation = zup_transaction::authenticate_cleanup(&store, &installation, transaction, node)
        .map_err(|reason| ExecError::refused(node.id.as_str(), reason))?;
    let completion = match &operation.candidate {
        CleanupCandidate::Generation(generation) => zup_transaction::collect_generation(
            &crate::collection::LinuxCollector(state.clone()),
            operation,
            generation,
            transaction.transaction_id,
        )
        .map_err(|reason| ExecError::refused(node.id.as_str(), reason))?,
        CleanupCandidate::Fixed(file) => remove_fixed(file)?,
        CleanupCandidate::History(history) => {
            zup_transaction::HistoryStorage(crate::history::LinuxHistoryFilesystem(state.clone()))
                .collect(history)
                .map_err(|reason| ExecError::refused(node.id.as_str(), reason))?
        }
    };
    Ok(OperationReceipt::Cleanup {
        intent: operation.fingerprint(),
        completion,
    })
}

fn parent(file: &zup_plan::FixedCleanup) -> Result<(crate::fs::OwnedDirectory, String), ExecError> {
    let path = crate::lowering::to_host_path(&file.path)?;
    crate::fs::verify_ancestor_trust(
        path.parent()
            .ok_or_else(|| ExecError::refused(file.path.as_str(), "retired file has no parent"))?,
        rustix::process::geteuid().as_raw(),
    )?;
    Ok(crate::fs::open_parent(&path)?)
}

fn remove_fixed(file: &zup_plan::FixedCleanup) -> Result<CleanupCompletion, ExecError> {
    let (parent, name) = parent(file)?;
    if parent.kind_or_absent(&name)?.is_none() {
        return Ok(CleanupCompletion::AlreadyAbsent);
    }
    let observed = parent.stream_regular(&name)?;
    if observed.object() != file.instance.object
        || observed.size != file.size
        || observed.sha256 != file.sha256
        || observed.uid != rustix::process::geteuid().as_raw()
        || observed.mode & 0o022 != 0
    {
        return Err(ExecError::refused(
            file.path.as_str(),
            "retired file differs from its protected native instance",
        ));
    }
    let fence = match rustix::fs::openat(
        parent.as_fd(),
        &name,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::TXTBSY) => {
            return Ok(CleanupCompletion::Deferred {
                reason: CleanupDeferral::InUse,
            });
        }
        Err(rustix::io::Errno::ACCESS | rustix::io::Errno::PERM | rustix::io::Errno::ROFS) => {
            return Ok(CleanupCompletion::Deferred {
                reason: CleanupDeferral::FenceUnavailable,
            });
        }
        Err(error) => return Err(ExecError::refused(file.path.as_str(), error.to_string())),
    };
    let identity = rustix::fs::fstat(&fence)
        .map_err(|error| ExecError::refused(file.path.as_str(), error.to_string()))?;
    let named = parent.stat_child(&name)?;
    if (identity.st_dev, identity.st_ino) != (observed.dev, observed.ino)
        || (named.dev, named.ino) != (observed.dev, observed.ino)
    {
        return Err(ExecError::refused(
            file.path.as_str(),
            "retired file identity changed before removal",
        ));
    }
    parent.remove_file(&name)?;
    parent.sync()?;
    if parent.kind_or_absent(&name)?.is_some() {
        return Err(ExecError::refused(
            file.path.as_str(),
            "retired slot is occupied after removal",
        ));
    }
    drop(fence);
    Ok(CleanupCompletion::Removed)
}

pub(crate) fn verify(
    state: &crate::state::StateAuthority,
    transaction: zup_core::TransactionId,
    node: &DeploymentNode,
    receipt: &OperationReceipt,
) -> Result<(), ExecError> {
    let refused = || {
        ExecError::refused(
            node.id.as_str(),
            "cleanup receipt differs from its native operation",
        )
    };
    let zup_plan::NodeKind::Cleanup { operation } = &node.kind else {
        return Err(refused());
    };
    let OperationReceipt::Cleanup { intent, completion } = receipt else {
        return Err(refused());
    };
    if intent != &operation.fingerprint() {
        return Err(refused());
    }
    if let CleanupCandidate::History(history) = &operation.candidate {
        return zup_transaction::HistoryStorage(crate::history::LinuxHistoryFilesystem(
            state.clone(),
        ))
        .verify(history, *completion)
        .map_err(|reason| ExecError::refused(node.id.as_str(), reason));
    }
    if let CleanupCandidate::Generation(generation) = &operation.candidate {
        return zup_transaction::verify_generation_collection(
            &crate::collection::LinuxCollector(state.clone()),
            operation,
            generation,
            transaction,
            *completion,
        )
        .map_err(|reason| ExecError::refused(node.id.as_str(), reason));
    }
    match completion {
        CleanupCompletion::Removed | CleanupCompletion::AlreadyAbsent => match &operation.candidate
        {
            CleanupCandidate::Fixed(file) => {
                let (parent, name) = parent(file)?;
                if parent.kind_or_absent(&name)?.is_some() {
                    return Err(refused());
                }
            }
            CleanupCandidate::Generation(_) | CleanupCandidate::History(_) => return Err(refused()),
        },
        CleanupCompletion::Deferred { .. } => {}
    }
    Ok(())
}
