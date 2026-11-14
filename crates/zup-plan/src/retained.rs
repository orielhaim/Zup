use std::collections::BTreeMap;

use thiserror::Error;
use zup_core::{ActivationRevision, PayloadPlacement, Template};

use crate::{
    DesiredFile, DesiredState, GenerationPreparation, GenerationPublication, GenerationSource,
    InstallationRecord, OwnedResource, PlannedGeneration,
};

#[derive(Debug, Error)]
pub enum RetainedDeploymentError {
    #[error("installation record: {0}")]
    Record(#[from] crate::InstallationRecordError),
    #[error("activation {0} is not retained by an installed application")]
    Missing(ActivationRevision),
    #[error("retained deployment does not contain the required owned file {0:?}")]
    File(zup_core::ResourceKey),
    #[error("retained deployment path is invalid: {0}")]
    Path(String),
    #[error(transparent)]
    Revision(#[from] zup_core::DeploymentIdentityError),
}

impl InstallationRecord {
    pub fn retained_deployment(
        &self,
        revision: ActivationRevision,
    ) -> Result<DesiredState, RetainedDeploymentError> {
        self.validate()?;
        let snapshot = self
            .retained
            .iter()
            .find(|snapshot| self.is_installed() && snapshot.revision == revision)
            .ok_or(RetainedDeploymentError::Missing(revision))?;
        let requirements = &snapshot.requirements;
        let template = |path: &zup_core::TargetPath| {
            Template::parse(path.as_str())
                .map_err(|error| RetainedDeploymentError::Path(error.to_string()))
        };
        let mut sources = BTreeMap::new();
        let files = requirements
            .files
            .iter()
            .map(|file| {
                let (placement, source_relative) = match &file.placement {
                    PayloadPlacement::Generation { path } => {
                        sources.insert(
                            path.clone(),
                            GenerationSource {
                                key: file.key.clone(),
                                source_relative: path.clone(),
                            },
                        );
                        (
                            PayloadPlacement::Generation {
                                path: Template::parse("${payload}")
                                    .expect("static template")
                                    .join_relative(path),
                            },
                            path.clone(),
                        )
                    }
                    PayloadPlacement::Fixed { path, replacement } => {
                        let Some(OwnedResource::File {
                            source_relative,
                            sha256,
                            size,
                            ..
                        }) = snapshot.resources.get(&file.key)
                        else {
                            return Err(RetainedDeploymentError::File(file.key.clone()));
                        };
                        if *sha256 != file.sha256 || *size != file.size {
                            return Err(RetainedDeploymentError::File(file.key.clone()));
                        }
                        (
                            PayloadPlacement::Fixed {
                                path: path.clone(),
                                replacement: *replacement,
                            },
                            source_relative.clone(),
                        )
                    }
                };
                Ok(DesiredFile {
                    key: file.key.clone(),
                    placement,
                    source_relative,
                    size: file.size,
                    sha256: file.sha256,
                    executable: file.executable,
                    privilege: file.privilege,
                })
            })
            .collect::<Result<Vec<_>, RetainedDeploymentError>>()?;
        let generation = snapshot
            .materialization
            .as_ref()
            .map(|materialization| {
                Ok::<_, RetainedDeploymentError>(PlannedGeneration {
                    installation: self.id,
                    id: materialization.id(),
                    descriptor: materialization.descriptor().clone(),
                    directory: template(materialization.directory())?,
                    publication: GenerationPublication::Application {
                        previous: Some(self.current.selection()),
                        revision: self.current.revision.next()?,
                    },
                    preparation: GenerationPreparation::ReuseVerified,
                    sources,
                })
            })
            .transpose()?;
        let maintenance = snapshot
            .maintenance
            .as_ref()
            .map(|materialization| {
                Ok::<_, RetainedDeploymentError>(PlannedGeneration {
                    installation: self.id,
                    id: materialization.id(),
                    descriptor: materialization.descriptor().clone(),
                    directory: template(materialization.directory())?,
                    publication: GenerationPublication::Maintenance,
                    preparation: GenerationPreparation::ReuseVerified,
                    sources: BTreeMap::new(),
                })
            })
            .transpose()?;
        Ok(DesiredState {
            generation_readers: requirements.generation_readers,
            requirements: (),
            installation: self.id,
            generation,
            maintenance,
            app: requirements.app.clone(),
            target: self.target,
            scope: self.scope,
            install_directory: template(&self.directory)?,
            data: requirements.data.clone(),
            selected_components: snapshot.selected_components.clone(),
            prerequisites: requirements.prerequisites.clone(),
            files,
            launchers: requirements.launchers.clone(),
            path_entries: requirements.path_entries.clone(),
            services: requirements.services.clone(),
            protocols: requirements.protocols.clone(),
            file_associations: requirements.file_associations.clone(),
            preset: snapshot.preset.clone(),
        })
    }
}
