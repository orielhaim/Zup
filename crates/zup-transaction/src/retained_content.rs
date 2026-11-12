use zup_core::{NativeObjectIdentity, TargetPath};
use zup_plan::{DeploymentNode, InstallationRecord, NodeKind, OwnedResource};

use crate::{
    FilesystemTransactionStore, OperationReceipt, ReconciliationError, TransactionPhase,
    TransactionRecord, TransactionStore,
};

pub struct RetainedFileSource {
    pub path: TargetPath,
    pub object: NativeObjectIdentity,
}

pub fn retained_file_sources(
    store: &FilesystemTransactionStore,
    installation: &InstallationRecord,
    transaction: &TransactionRecord,
    node: &DeploymentNode,
) -> Result<Vec<RetainedFileSource>, ReconciliationError> {
    if installation.validate().is_err()
        || installation.id != transaction.plan.installation
        || installation.app_id != transaction.app_id
        || installation.scope != transaction.scope
        || installation.target != transaction.target
        || !transaction.plan.nodes.contains(node)
    {
        return Err(ReconciliationError::Generations);
    }
    let NodeKind::StageFile { key } = &node.kind else {
        return Err(ReconciliationError::Generations);
    };
    let roots = std::iter::once(&installation.current)
        .chain(&installation.retained)
        .filter_map(|snapshot| match snapshot.resources.get(key)? {
            OwnedResource::File {
                instance,
                destination,
                sha256,
                size,
                ..
            } if node.meta.expected_sha256 == Some(*sha256)
                && node.meta.expected_size == Some(*size) =>
            {
                Some((instance.object, destination.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut sources = Vec::new();
    let mut add = |path: TargetPath, object| {
        if roots.iter().any(|(root, _)| *root == object)
            && !sources.iter().any(|source: &RetainedFileSource| {
                source.object == object && source.path.equivalent(&path)
            })
        {
            sources.push(RetainedFileSource { path, object });
        }
    };
    for (object, path) in &roots {
        add(path.clone(), *object);
    }
    if roots.is_empty() {
        return Ok(sources);
    }
    for id in store.list_ids()? {
        let record = store.load(&id)?;
        if record.phase != TransactionPhase::Committed
            || record.app_id != installation.app_id
            || record.scope != installation.scope
            || record.target != installation.target
            || record.plan.installation != installation.id
        {
            continue;
        }
        for effect in &record.plan.nodes {
            if !matches!(&effect.kind, NodeKind::FileMutation { key: candidate, delta: zup_plan::FileOperationKind::Replace, .. } | NodeKind::FileRemoval { key: candidate } if candidate == key)
            {
                continue;
            }
            match record.receipt(&effect.id) {
                Some(OperationReceipt::ReplaceFile {
                    previous_object,
                    previous_size,
                    previous_sha256,
                    backup_path,
                    ..
                }) if node.meta.expected_size == Some(*previous_size)
                    && node.meta.expected_sha256 == Some(*previous_sha256) =>
                {
                    add(
                        TargetPath::new(record.target, backup_path)
                            .map_err(|_| ReconciliationError::Target)?,
                        *previous_object,
                    );
                }
                Some(OperationReceipt::RemoveFile {
                    object,
                    size,
                    sha256,
                    backup_path,
                    ..
                }) if node.meta.expected_size == Some(*size)
                    && node.meta.expected_sha256 == Some(*sha256) =>
                {
                    add(backup_path.clone(), *object)
                }
                _ => {}
            }
        }
    }
    Ok(sources)
}
