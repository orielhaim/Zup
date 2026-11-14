use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zup_core::{ResourceKey, Sha256Digest, Target, TargetOperatingSystem, TargetPath, hash_bytes};

use crate::{DeploymentNode, DeploymentPlanError, NodeKind, OperationId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeCacheKind {
    Mime,
    DesktopApplications,
}

impl NativeCacheKind {
    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Mime => "update-mime-database",
            Self::DesktopApplications => "update-desktop-database",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Mime => "MIME type cache",
            Self::DesktopApplications => "Desktop application cache",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCacheOperation {
    pub kind: NativeCacheKind,
    pub directory: TargetPath,
    pub sources: BTreeSet<ResourceKey>,
}

impl NativeCacheOperation {
    pub fn id(&self) -> OperationId {
        let kind = match self.kind {
            NativeCacheKind::Mime => "mime",
            NativeCacheKind::DesktopApplications => "desktop-applications",
        };
        OperationId::new(format!(
            "op:native-cache:{kind}:{}",
            hash_bytes(self.directory.identity().as_bytes())
        ))
    }

    pub fn fingerprint(&self) -> Sha256Digest {
        hash_bytes(
            &serde_json::to_vec(&("zup.native-cache.v1", self))
                .expect("native cache intent serializes"),
        )
    }

    pub fn validate(&self, target: Target) -> Result<(), DeploymentPlanError> {
        if target.operating_system() != TargetOperatingSystem::Linux
            || self.directory.target() != &target
            || self.directory.parent().is_none()
            || self.sources.is_empty()
        {
            return Err(DeploymentPlanError::InvalidNode {
                id: self.id().to_string(),
                reason: "native cache regeneration requires a Linux directory and contributing source resources".into(),
            });
        }
        Ok(())
    }

    pub fn includes_source(&self, path: &TargetPath) -> bool {
        let (directory, suffix) = match self.kind {
            NativeCacheKind::Mime => (
                self.directory
                    .join("packages")
                    .expect("static relative directory"),
                ".xml",
            ),
            NativeCacheKind::DesktopApplications => (self.directory.clone(), ".desktop"),
        };
        path.parent().as_ref() == Some(&directory)
            && path.file_name().is_some_and(|name| name.ends_with(suffix))
    }

    pub fn source_nodes<'a>(
        &self,
        nodes: &'a [DeploymentNode],
    ) -> Result<Vec<&'a DeploymentNode>, DeploymentPlanError> {
        let sources: Vec<_> = nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    NodeKind::FileMutation { .. } | NodeKind::FileRemoval { .. }
                ) && node
                    .file_destination()
                    .is_some_and(|path| self.includes_source(path))
            })
            .collect();
        let keys: BTreeSet<_> = sources
            .iter()
            .filter_map(|node| match &node.kind {
                NodeKind::FileMutation { key, .. } | NodeKind::FileRemoval { key } => {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect();
        if keys != self.sources {
            return Err(DeploymentPlanError::InvalidNode {
                id: self.id().to_string(),
                reason:
                    "native cache sources do not match the changed resources in its input directory"
                        .into(),
            });
        }
        Ok(sources)
    }
}
