use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use thiserror::Error;
use wasmtime::{Engine, Precompiled};
use zup_bundle::{
    AcquiredPayloadSource, Package, PackageError, PayloadError, PluginArtifact, PortableBuildPlan,
};
use zup_core::{PluginId, Sha256Digest, Target};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, ContractError, EngineError, HOST_TARGET, MAX_AOT_BYTES, PLUGIN_API_VERSION,
    PluginEngine, ValidatedComponent, WASMTIME_VERSION, wit_package_digest,
};

#[derive(Debug, Error)]
pub enum LoadError {
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("expected target `{expected}` does not match the host compile target `{host}`")]
    HostTargetMismatch { expected: Target, host: Target },
    #[error("plugin artifact target `{found}` does not match expected target `{expected}`")]
    TargetMismatch { expected: Target, found: Target },
    #[error("plugin `{plugin_id}` is missing from the package artifact set")]
    MissingArtifact { plugin_id: PluginId },
    #[error("plugin artifact set has an unexpected number of entries")]
    ArtifactCount,
    #[error("plugin artifact `{plugin_id}` is duplicated")]
    DuplicateArtifact { plugin_id: PluginId },
    #[error("installer plugin `{plugin_id}` is duplicated")]
    DuplicateInstallerPlugin { plugin_id: PluginId },
    #[error("plugin `{plugin_id}` has unsupported {field}: {found}")]
    MetadataMismatch {
        plugin_id: PluginId,
        field: &'static str,
        found: String,
    },
    #[error("plugin `{plugin_id}` AOT bytes are not a precompiled component")]
    NotAComponent { plugin_id: PluginId },
    #[error("plugin `{plugin_id}` AOT bytes are not owned by the verified package")]
    UnverifiedAot { plugin_id: PluginId },
}

pub(crate) struct LoadedPlugin {
    pub(crate) bytes: Vec<u8>,
    pub(crate) component: Option<ValidatedComponent>,
}

pub(crate) struct LoadedBundle {
    pub(crate) engine: PluginEngine,
    pub(crate) target: Target,
    pub(crate) plugins: BTreeMap<PluginId, LoadedPlugin>,
}

pub(crate) fn load_bundle(
    package: Package,
    expected_target: &Target,
) -> Result<LoadedBundle, LoadError> {
    check_target(package.plan(), expected_target)?;
    load_sources(
        &package.plan().installer,
        &package.plan().plugins,
        expected_target,
        |artifact| Ok(package.plugin_aot(&artifact.plugin_id)?),
    )
}

pub(crate) fn load_acquired(
    plan: &PortableBuildPlan,
    source: &AcquiredPayloadSource,
    expected_target: &Target,
) -> Result<LoadedBundle, LoadError> {
    check_target(plan, expected_target)?;
    load_sources(
        &plan.installer,
        &plan.plugins,
        expected_target,
        |artifact| Ok(source.read_blob(&artifact.blob)?),
    )
}

pub(crate) fn load_compiled(
    installer: &zup_core::Installer,
    artifacts: &[zup_bundle::CompiledPluginArtifact],
) -> Result<LoadedBundle, LoadError> {
    let metadata = artifacts
        .iter()
        .map(|artifact| artifact.metadata().clone())
        .collect::<Vec<_>>();
    load_sources(installer, &metadata, &host_target(), |metadata| {
        artifacts
            .iter()
            .find(|artifact| artifact.metadata().plugin_id == metadata.plugin_id)
            .map(|artifact| artifact.bytes().to_vec())
            .ok_or_else(|| LoadError::MissingArtifact {
                plugin_id: metadata.plugin_id.clone(),
            })
    })
}

fn check_target(plan: &PortableBuildPlan, expected: &Target) -> Result<(), LoadError> {
    if plan.installer.target != *expected {
        return Err(LoadError::TargetMismatch {
            expected: *expected,
            found: plan.installer.target,
        });
    }
    Ok(())
}

fn load_sources(
    installer: &zup_core::Installer,
    metadata: &[PluginArtifact],
    expected_target: &Target,
    mut read: impl FnMut(&PluginArtifact) -> Result<Vec<u8>, LoadError>,
) -> Result<LoadedBundle, LoadError> {
    let host = host_target();
    if expected_target != &host {
        return Err(LoadError::HostTargetMismatch {
            expected: *expected_target,
            host,
        });
    }
    let engine = PluginEngine::new(expected_target.rust_triple())?;
    let expected_fingerprint = Sha256Digest::from_bytes(*engine.fingerprint().as_bytes());
    let artifacts = exact_artifacts(installer, metadata)?;
    let mut plugins = BTreeMap::new();
    for (plugin_id, artifact) in artifacts {
        verify_metadata(&plugin_id, artifact, expected_target, expected_fingerprint)?;
        let bytes = read(artifact)?;
        verify_aot(&plugin_id, artifact, &bytes)?;
        plugins.insert(
            plugin_id,
            LoadedPlugin {
                bytes,
                component: None,
            },
        );
    }
    Ok(LoadedBundle {
        engine,
        target: installer.target,
        plugins,
    })
}

impl LoadedBundle {
    pub(crate) fn component(
        &mut self,
        plugin_id: &PluginId,
    ) -> Result<&ValidatedComponent, LoadError> {
        let plugin = self
            .plugins
            .get_mut(plugin_id)
            .ok_or_else(|| LoadError::MissingArtifact {
                plugin_id: plugin_id.clone(),
            })?;
        if plugin.component.is_none() {
            // SAFETY: load_sources verified these bytes and their engine metadata against the authenticated plan.
            plugin.component = Some(unsafe { self.engine.validate_trusted_aot(&plugin.bytes)? });
        }
        Ok(plugin.component.as_ref().expect("component loaded above"))
    }
}

fn verify_aot(
    plugin_id: &PluginId,
    artifact: &PluginArtifact,
    bytes: &[u8],
) -> Result<(), LoadError> {
    if bytes.len() as u64 != artifact.aot_size
        || Sha256Digest::from_bytes(Sha256::digest(bytes).into()) != artifact.aot_sha256
    {
        return Err(LoadError::UnverifiedAot {
            plugin_id: plugin_id.clone(),
        });
    }
    if Engine::detect_precompiled(bytes) != Some(Precompiled::Component) {
        return Err(LoadError::NotAComponent {
            plugin_id: plugin_id.clone(),
        });
    }
    Ok(())
}

fn host_target() -> Target {
    Target::parse(HOST_TARGET).expect("host target is a valid triple")
}

fn exact_artifacts<'a>(
    installer: &zup_core::Installer,
    metadata: &'a [PluginArtifact],
) -> Result<BTreeMap<PluginId, &'a PluginArtifact>, LoadError> {
    let mut expected = BTreeMap::<String, PluginId>::new();
    for binding in &installer.plugins {
        let key = binding.id.as_str().to_ascii_lowercase();
        if let Some(previous) = expected.insert(key, binding.id.clone()) {
            return Err(LoadError::DuplicateInstallerPlugin {
                plugin_id: previous,
            });
        }
    }

    let mut actual = BTreeMap::<String, &PluginArtifact>::new();
    for artifact in metadata {
        let key = artifact.plugin_id.as_str().to_ascii_lowercase();
        if actual.insert(key.clone(), artifact).is_some() {
            return Err(LoadError::DuplicateArtifact {
                plugin_id: artifact.plugin_id.clone(),
            });
        }
    }
    if actual.len() != expected.len() {
        return Err(LoadError::ArtifactCount);
    }

    let mut canonical = BTreeMap::new();
    for (key, plugin_id) in expected {
        let artifact = actual.get(&key).ok_or_else(|| LoadError::MissingArtifact {
            plugin_id: plugin_id.clone(),
        })?;
        if artifact.plugin_id != plugin_id {
            return Err(LoadError::MetadataMismatch {
                plugin_id,
                field: "plugin id",
                found: artifact.plugin_id.to_string(),
            });
        }
        canonical.insert(plugin_id, *artifact);
    }
    Ok(canonical)
}

fn verify_metadata(
    plugin_id: &PluginId,
    artifact: &PluginArtifact,
    expected_target: &Target,
    expected_fingerprint: Sha256Digest,
) -> Result<(), LoadError> {
    if artifact.target != *expected_target {
        return Err(LoadError::TargetMismatch {
            expected: *expected_target,
            found: artifact.target,
        });
    }
    if artifact.wasmtime_version != WASMTIME_VERSION {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "Wasmtime version",
            found: artifact.wasmtime_version.clone(),
        });
    }
    if artifact.aot_format_version != AOT_FORMAT_VERSION {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "AOT format version",
            found: artifact.aot_format_version.to_string(),
        });
    }
    if artifact.plugin_api_version != PLUGIN_API_VERSION {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "plugin API version",
            found: artifact.plugin_api_version.clone(),
        });
    }
    if artifact.wit_digest != Sha256Digest::from_bytes(wit_package_digest()) {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "WIT digest",
            found: artifact.wit_digest.to_hex(),
        });
    }
    if artifact.engine_fingerprint != expected_fingerprint {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "engine fingerprint",
            found: artifact.engine_fingerprint.to_hex(),
        });
    }
    if artifact.aot_sha256 != artifact.blob {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "AOT digest binding",
            found: artifact.blob.to_hex(),
        });
    }
    if artifact.aot_size == 0
        || artifact.aot_size > u64::try_from(MAX_AOT_BYTES).unwrap_or(u64::MAX)
    {
        return Err(LoadError::MetadataMismatch {
            plugin_id: plugin_id.clone(),
            field: "AOT size",
            found: artifact.aot_size.to_string(),
        });
    }
    artifact.validate().map_err(LoadError::Package)?;
    Ok(())
}
