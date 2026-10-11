use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use zup_core::{
    Frontend, Install, ResolvedTargetConfig, TargetOverrides, TargetProfile, TargetProfileId,
    TargetTriple,
};

use crate::error::ManifestError;
use crate::model::{ArtifactKind, Manifest, Targeted};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Component,
    Prerequisite,
    Plugin,
    File,
    Launcher,
    PathEntry,
    Service,
    Protocol,
    FileAssociation,
    Artifact,
}

impl ResourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Prerequisite => "prerequisite",
            Self::Plugin => "plugin",
            Self::File => "file",
            Self::Launcher => "launcher",
            Self::PathEntry => "path entry",
            Self::Service => "service",
            Self::Protocol => "protocol",
            Self::FileAssociation => "file association",
            Self::Artifact => "artifact",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub(crate) fn validate_target_matrix(manifest: &Manifest) -> Result<(), ManifestError> {
    if manifest.build.targets.is_empty() {
        return Err(ManifestError::EmptyTargetMatrix {
            src: None,
            span: None,
        });
    }
    Ok(())
}

pub(crate) fn validate_artifacts(manifest: &Manifest) -> Result<(), ManifestError> {
    for (id, artifact) in &manifest.build.artifacts {
        if artifact.targets.is_empty() {
            return Err(ManifestError::EmptyArtifact {
                artifact: id.to_string(),
                src: None,
                span: None,
            });
        }
        for profile in &artifact.targets {
            if !manifest.build.targets.contains_key(profile) {
                return Err(ManifestError::UnknownTargetProfileReference {
                    resource: ResourceKind::Artifact,
                    profile: profile.to_string(),
                    src: None,
                    span: None,
                });
            }
        }
        if artifact.kind == ArtifactKind::Single && artifact.targets.len() > 1 {
            return Err(ManifestError::ArtifactTargetCount {
                artifact: id.to_string(),
                count: artifact.targets.len(),
                src: None,
                span: None,
            });
        }
        if let Some(channel) = &artifact.channel
            && channel.trim().is_empty()
        {
            return Err(ManifestError::EmptyArtifactChannel {
                artifact: id.to_string(),
                src: None,
                span: None,
            });
        }
        if let Some(output) = &artifact.output
            && (output.is_empty()
                || output.contains('/')
                || output.contains('\\')
                || output.contains(':'))
        {
            return Err(ManifestError::ArtifactOutput {
                artifact: id.to_string(),
                output: output.clone(),
                src: None,
                span: None,
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_target_references(manifest: &Manifest) -> Result<(), ManifestError> {
    let profiles = &manifest.build.targets;
    validate_resource_targets(ResourceKind::Component, &manifest.components, profiles)?;
    validate_resource_targets(
        ResourceKind::Prerequisite,
        &manifest.prerequisites,
        profiles,
    )?;
    validate_resource_targets(ResourceKind::Plugin, &manifest.plugins, profiles)?;
    validate_resource_targets(ResourceKind::File, &manifest.files, profiles)?;
    validate_resource_targets(ResourceKind::Launcher, &manifest.launchers, profiles)?;
    validate_resource_targets(ResourceKind::PathEntry, &manifest.path, profiles)?;
    validate_resource_targets(ResourceKind::Service, &manifest.services, profiles)?;
    validate_resource_targets(ResourceKind::Protocol, &manifest.protocols, profiles)?;
    validate_resource_targets(
        ResourceKind::FileAssociation,
        &manifest.file_associations,
        profiles,
    )?;
    Ok(())
}

fn validate_resource_targets<T>(
    resource: ResourceKind,
    declarations: &[Targeted<T>],
    profiles: &BTreeMap<TargetProfileId, TargetProfile>,
) -> Result<(), ManifestError> {
    for declaration in declarations {
        for profile in &declaration.targets {
            if !profiles.contains_key(profile) {
                return Err(ManifestError::UnknownTargetProfileReference {
                    resource,
                    profile: profile.to_string(),
                    src: None,
                    span: None,
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn resolve_target_config(
    profile: &TargetProfileId,
    config: &TargetProfile,
    common_frontend: Frontend,
    common_install: &Install,
    overrides: &TargetOverrides,
) -> ResolvedTargetConfig {
    let mut install = config
        .install
        .clone()
        .unwrap_or_else(|| common_install.clone());
    if let Some(directory) = &overrides.install_directory {
        if install.scope.allows_user() {
            install.directory.user = Some(directory.clone());
        }
        if install.scope.allows_machine() {
            install.directory.machine = Some(directory.clone());
        }
    }
    ResolvedTargetConfig {
        profile: profile.clone(),
        target: config.target.clone(),
        source: overrides
            .source
            .clone()
            .unwrap_or_else(|| config.source.clone()),
        frontend: overrides
            .frontend
            .or(config.frontend)
            .unwrap_or(common_frontend),
        install,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetOverrideSet {
    shared: TargetOverrides,
    profiles: BTreeMap<TargetProfileId, TargetOverrides>,
}

impl TargetOverrideSet {
    pub fn get(&self, profile: &TargetProfileId) -> &TargetOverrides {
        self.profiles.get(profile).unwrap_or(&self.shared)
    }

    pub fn apply(&mut self, profile: TargetProfileId, overrides: TargetOverrides) {
        self.profiles.insert(profile, overrides);
    }

    pub fn uniform(overrides: TargetOverrides) -> Self {
        Self {
            shared: overrides,
            profiles: BTreeMap::new(),
        }
    }
}

pub fn select_targets(
    manifest: &Manifest,
    selectors: &[&str],
    overrides: &TargetOverrides,
) -> Result<Vec<ResolvedTargetConfig>, ManifestError> {
    select_targets_with(
        manifest,
        selectors,
        &TargetOverrideSet::uniform(overrides.clone()),
    )
}

pub fn select_targets_with(
    manifest: &Manifest,
    selectors: &[&str],
    overrides: &TargetOverrideSet,
) -> Result<Vec<ResolvedTargetConfig>, ManifestError> {
    let selected = resolve_selection(manifest, selectors)?;

    Ok(manifest
        .build
        .targets
        .iter()
        .filter(|(profile, _)| selected.contains(*profile))
        .map(|(profile, config)| {
            resolve_target_config(
                profile,
                config,
                manifest.frontend,
                &manifest.install,
                overrides.get(profile),
            )
        })
        .collect())
}

fn resolve_selection(
    manifest: &Manifest,
    selectors: &[&str],
) -> Result<BTreeSet<TargetProfileId>, ManifestError> {
    validate_target_matrix(manifest)?;
    validate_target_references(manifest)?;
    validate_artifacts(manifest)?;

    let mut canonical_targets = BTreeMap::<TargetTriple, &TargetProfileId>::new();
    for (profile, config) in &manifest.build.targets {
        if let Some(conflicts_with) = canonical_targets.insert(config.target.clone(), profile) {
            return Err(ManifestError::DuplicateTarget {
                profile: profile.to_string(),
                conflicts_with: conflicts_with.to_string(),
                target: config.target.to_string(),
                src: None,
                span: None,
            });
        }
    }

    let available = manifest
        .build
        .targets
        .keys()
        .map(|profile| profile.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let unknown = |selector: &str| ManifestError::UnknownTargetSelector {
        selector: selector.to_owned(),
        available: available.clone(),
        src: None,
        span: None,
    };

    let mut selected = BTreeSet::new();
    if selectors.is_empty() {
        selected.extend(manifest.build.targets.keys().cloned());
    }
    for selector in selectors {
        let profile = if let Some((profile, _)) = manifest.build.targets.get_key_value(*selector) {
            profile.clone()
        } else if let Ok(target) = TargetTriple::parse(*selector) {
            manifest
                .build
                .targets
                .iter()
                .find_map(|(profile, config)| (config.target == target).then(|| profile.clone()))
                .ok_or_else(|| unknown(selector))?
        } else {
            return Err(unknown(selector));
        };
        selected.insert(profile);
    }
    Ok(selected)
}
