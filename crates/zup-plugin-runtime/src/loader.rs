use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use thiserror::Error;
use wasmtime::{Engine, Precompiled};
use zup_bundle::{Package, PackageError, PluginArtifact};
use zup_core::{PluginId, Sha256Digest, TargetTriple};
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
    Contract(#[from] ContractError),
    #[error("expected target `{expected}` does not match the host compile target `{host}`")]
    HostTargetMismatch {
        expected: TargetTriple,
        host: TargetTriple,
    },
    #[error("plugin artifact target `{found}` does not match expected target `{expected}`")]
    TargetMismatch {
        expected: TargetTriple,
        found: TargetTriple,
    },
    #[error("plugin `{plugin_id}` is missing from the package artifact set")]
    MissingArtifact { plugin_id: PluginId },
    #[error("plugin artifact set has an unexpected number of entries")]
    ArtifactCount,
    #[error("plugin artifact `{plugin_id}` is not declared by the installer")]
    ExtraArtifact { plugin_id: PluginId },
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
    pub(crate) artifact: PluginArtifact,
    pub(crate) component: Option<ValidatedComponent>,
}

pub(crate) struct LoadedBundle {
    pub(crate) package: Package,
    pub(crate) engine: PluginEngine,
    pub(crate) target: TargetTriple,
    pub(crate) plugins: BTreeMap<PluginId, LoadedPlugin>,
}

pub(crate) fn load_bundle(
    package: Package,
    expected_target: &TargetTriple,
) -> Result<LoadedBundle, LoadError> {
    if &package.plan().installer.target != expected_target {
        return Err(LoadError::TargetMismatch {
            expected: expected_target.clone(),
            found: package.plan().installer.target.clone(),
        });
    }
    let host = host_target();
    if expected_target != &host {
        return Err(LoadError::HostTargetMismatch {
            expected: expected_target.clone(),
            host,
        });
    }
    let engine = PluginEngine::new(expected_target.as_str())?;
    if engine.target() != expected_target.as_str() {
        return Err(LoadError::TargetMismatch {
            expected: expected_target.clone(),
            found: TargetTriple::parse(engine.target()).expect("engine target is a valid triple"),
        });
    }
    let expected_fingerprint = Sha256Digest::from_bytes(*engine.fingerprint().as_bytes());
    let artifacts = exact_artifacts(&package)?;
    let mut plugins = BTreeMap::new();
    for (plugin_id, artifact) in artifacts {
        verify_metadata(&plugin_id, artifact, expected_target, expected_fingerprint)?;
        let bytes = package.plugin_aot(&plugin_id)?;
        if bytes.len() as u64 != artifact.aot_size
            || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != artifact.aot_sha256
        {
            return Err(LoadError::UnverifiedAot { plugin_id });
        }
        if Engine::detect_precompiled(&bytes) != Some(Precompiled::Component) {
            return Err(LoadError::NotAComponent { plugin_id });
        }
        plugins.insert(
            plugin_id,
            LoadedPlugin {
                artifact: artifact.clone(),
                component: None,
            },
        );
    }
    Ok(LoadedBundle {
        package,
        engine,
        target: expected_target.clone(),
        plugins,
    })
}

impl LoadedBundle {
    pub(crate) fn component(
        &mut self,
        plugin_id: &PluginId,
    ) -> Result<&ValidatedComponent, LoadError> {
        if !self.plugins.contains_key(plugin_id) {
            return Err(LoadError::MissingArtifact {
                plugin_id: plugin_id.clone(),
            });
        }
        if self.plugins[plugin_id].component.is_none() {
            let artifact = self.plugins[plugin_id].artifact.clone();
            verify_metadata(
                plugin_id,
                &artifact,
                &self.target,
                Sha256Digest::from_bytes(*self.engine.fingerprint().as_bytes()),
            )?;
            let bytes = self.package.plugin_aot(plugin_id)?;
            if bytes.len() as u64 != artifact.aot_size
                || Sha256Digest::from_bytes(Sha256::digest(&bytes).into()) != artifact.aot_sha256
            {
                return Err(LoadError::UnverifiedAot {
                    plugin_id: plugin_id.clone(),
                });
            }
            if Engine::detect_precompiled(&bytes) != Some(Precompiled::Component) {
                return Err(LoadError::NotAComponent {
                    plugin_id: plugin_id.clone(),
                });
            }
            // SAFETY: the package accessor re-read and authenticated these exact bytes, and the engine metadata was validated at load.
            let component = unsafe { self.engine.validate_trusted_aot(&bytes)? };
            self.plugins
                .get_mut(plugin_id)
                .expect("plugin presence checked")
                .component = Some(component);
        }
        Ok(self.plugins[plugin_id]
            .component
            .as_ref()
            .expect("component loaded above"))
    }
}

fn host_target() -> TargetTriple {
    TargetTriple::parse(HOST_TARGET).expect("host target is a valid triple")
}

fn exact_artifacts(package: &Package) -> Result<BTreeMap<PluginId, &PluginArtifact>, LoadError> {
    let mut expected = BTreeMap::<String, PluginId>::new();
    for binding in &package.plan().installer.plugins {
        let key = binding.id.as_str().to_ascii_lowercase();
        if let Some(previous) = expected.insert(key, binding.id.clone()) {
            return Err(LoadError::DuplicateInstallerPlugin {
                plugin_id: previous,
            });
        }
    }

    let mut actual = BTreeMap::<String, &PluginArtifact>::new();
    for artifact in package.plugin_artifacts() {
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
    expected_target: &TargetTriple,
    expected_fingerprint: Sha256Digest,
) -> Result<(), LoadError> {
    if artifact.target != *expected_target {
        return Err(LoadError::TargetMismatch {
            expected: expected_target.clone(),
            found: artifact.target.clone(),
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
    let host = host_target();
    if artifact.target != host {
        return Err(LoadError::HostTargetMismatch {
            expected: artifact.target.clone(),
            host,
        });
    }
    artifact.validate().map_err(LoadError::Package)?;
    Ok(())
}
