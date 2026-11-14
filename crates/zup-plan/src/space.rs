use zup_core::DeploymentStorageEstimate;

use crate::{CleanupCandidate, DeploymentPlan, FilePrecondition, GenerationOperation, NodeKind};

impl DeploymentPlan {
    pub fn storage_estimate(&self) -> DeploymentStorageEstimate {
        let mut estimate = DeploymentStorageEstimate::default();
        for node in &self.nodes {
            match &node.kind {
                NodeKind::Generation {
                    operation: GenerationOperation::Materialize,
                    intent,
                } => {
                    estimate.new_generation_bytes = estimate
                        .new_generation_bytes
                        .saturating_add(intent.materialization.descriptor().size());
                }
                NodeKind::Generation {
                    operation: GenerationOperation::Verify,
                    intent,
                } => {
                    estimate.verified_reuse_bytes = estimate
                        .verified_reuse_bytes
                        .saturating_add(intent.materialization.descriptor().size());
                }
                NodeKind::StageFile { .. } => {
                    estimate.fixed_staging_bytes = estimate
                        .fixed_staging_bytes
                        .saturating_add(node.meta.expected_size.unwrap_or(0));
                }
                NodeKind::FileMutation { .. } => {
                    if let Some(FilePrecondition::Exact { size, .. }) = node.meta.file_precondition
                    {
                        estimate.retained_fixed_bytes =
                            estimate.retained_fixed_bytes.saturating_add(size);
                    }
                }
                NodeKind::FileRemoval { .. } => {
                    if let Some(removal) = &node.meta.removal {
                        estimate.retained_fixed_bytes =
                            estimate.retained_fixed_bytes.saturating_add(removal.size);
                    }
                }
                NodeKind::Cleanup { operation } => {
                    let bytes = match &operation.candidate {
                        CleanupCandidate::Generation(generation) => {
                            generation.materialization.descriptor().size()
                        }
                        CleanupCandidate::Fixed(file) => file.size,
                        CleanupCandidate::History(history) => match &history.object {
                            crate::HistoryObject::Journal { members, .. } => members
                                .values()
                                .fold(0u64, |total, file| total.saturating_add(file.size)),
                            crate::HistoryObject::File { observed, .. } => observed.size,
                        },
                    };
                    estimate.cleanup_candidate_bytes =
                        estimate.cleanup_candidate_bytes.saturating_add(bytes);
                }
                _ => {}
            }
        }
        estimate
    }
}
