#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;
pub use zup_build::MAX_PLUGIN_SOURCE_BYTES;
use zup_build::{BuildPlan, ResolvedPlugin};
use zup_bundle::{CompiledPluginArtifact, MAX_PLUGIN_AOT_TOTAL_BYTES, PluginArtifact};
use zup_core::{MAX_PLUGIN_ARTIFACTS, Sha256Digest};
use zup_plugin_contract::{
    AOT_FORMAT_VERSION, ContractError, EngineError, PLUGIN_API_VERSION, PluginEngine,
    WASMTIME_VERSION, wit_package_digest,
};

#[derive(Debug, Error)]
pub enum PluginBuildError {
    #[error("failed to read plugin source `{path}`: {source}")]
    SourceRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin source `{path}` is not a regular file")]
    SourceNotRegular { path: String },
    #[error("plugin source `{path}` is a symlink")]
    SourceSymlink { path: String },
    #[error("plugin source `{path}` is {size} bytes; the limit is {limit} bytes")]
    SourceTooLarge { path: String, size: u64, limit: u64 },
    #[error("plugin source `{path}` does not match its resolved build metadata")]
    SourceChanged { path: String },
    #[error("resolved plugin sources do not match the installer plugin bindings")]
    PlanMismatch,
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Artifact(#[from] zup_bundle::BundleError),
}

pub fn compile_plugins(
    plan: &BuildPlan,
    target: &str,
) -> Result<Vec<CompiledPluginArtifact>, PluginBuildError> {
    if plan.plugins.len() > MAX_PLUGIN_ARTIFACTS
        || plan.installer.plugins.len() > MAX_PLUGIN_ARTIFACTS
    {
        return Err(zup_bundle::BundleError::TooManyPluginArtifacts {
            count: plan.plugins.len().max(plan.installer.plugins.len()),
            limit: MAX_PLUGIN_ARTIFACTS,
        }
        .into());
    }
    if plan.plugins.len() != plan.installer.plugins.len()
        || plan
            .plugins
            .iter()
            .zip(&plan.installer.plugins)
            .any(|(resolved, binding)| resolved.id != binding.id)
    {
        return Err(PluginBuildError::PlanMismatch);
    }
    let engine = PluginEngine::new(target)?;
    compile_with_engine(&plan.plugins, target, &engine)
}

fn add_aot_size(total: &mut u64, size: u64) -> Result<(), zup_bundle::BundleError> {
    *total = total
        .checked_add(size)
        .ok_or(zup_bundle::BundleError::PluginAotTooLarge {
            size: u64::MAX,
            limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
        })?;
    if *total > MAX_PLUGIN_AOT_TOTAL_BYTES {
        return Err(zup_bundle::BundleError::PluginAotTooLarge {
            size: *total,
            limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
        });
    }
    Ok(())
}

fn compile_with_engine(
    plugins: &[ResolvedPlugin],
    target: &str,
    engine: &PluginEngine,
) -> Result<Vec<CompiledPluginArtifact>, PluginBuildError> {
    let mut artifacts = Vec::with_capacity(plugins.len());
    let mut aot_total = 0u64;
    for resolved in plugins {
        if resolved.size > MAX_PLUGIN_SOURCE_BYTES {
            return Err(PluginBuildError::SourceTooLarge {
                path: resolved.source.display().to_string(),
                size: resolved.size,
                limit: MAX_PLUGIN_SOURCE_BYTES,
            });
        }
        let source = read_source(&resolved.source)?;
        if source.len() as u64 != resolved.size
            || Sha256Digest::from_bytes(Sha256::digest(&source).into()) != resolved.sha256
        {
            return Err(PluginBuildError::SourceChanged {
                path: resolved.source.display().to_string(),
            });
        }
        let aot = engine.precompile_component(&source)?;
        let aot_size =
            u64::try_from(aot.len()).map_err(|_| zup_bundle::BundleError::PluginAotTooLarge {
                size: u64::MAX,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            })?;
        add_aot_size(&mut aot_total, aot_size)?;
        engine.verify_precompiled(&aot)?;
        let aot_sha256 = Sha256Digest::from_bytes(Sha256::digest(&aot).into());
        let metadata = PluginArtifact {
            plugin_id: resolved.id.clone(),
            source_size: resolved.size,
            source_sha256: resolved.sha256,
            target: target.to_owned(),
            wasmtime_version: WASMTIME_VERSION.to_owned(),
            aot_format_version: AOT_FORMAT_VERSION,
            plugin_api_version: PLUGIN_API_VERSION.to_owned(),
            wit_digest: Sha256Digest::from_bytes(wit_package_digest()),
            engine_fingerprint: Sha256Digest::from_bytes(*engine.fingerprint().as_bytes()),
            aot_size: aot.len() as u64,
            aot_sha256,
            blob: aot_sha256,
        };
        artifacts.push(CompiledPluginArtifact::new(metadata, aot)?);
    }
    Ok(artifacts)
}

fn read_source(path: &Path) -> Result<Vec<u8>, PluginBuildError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| PluginBuildError::SourceRead {
        path: path.display().to_string(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(PluginBuildError::SourceSymlink {
            path: path.display().to_string(),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(PluginBuildError::SourceNotRegular {
            path: path.display().to_string(),
        });
    }
    if metadata.len() > MAX_PLUGIN_SOURCE_BYTES {
        return Err(PluginBuildError::SourceTooLarge {
            path: path.display().to_string(),
            size: metadata.len(),
            limit: MAX_PLUGIN_SOURCE_BYTES,
        });
    }
    let file = File::open(path).map_err(|source| PluginBuildError::SourceRead {
        path: path.display().to_string(),
        source,
    })?;
    let mut source = Vec::new();
    source
        .try_reserve_exact(usize::try_from(metadata.len()).unwrap_or(usize::MAX))
        .map_err(|_| PluginBuildError::SourceTooLarge {
            path: path.display().to_string(),
            size: metadata.len(),
            limit: MAX_PLUGIN_SOURCE_BYTES,
        })?;
    file.take(MAX_PLUGIN_SOURCE_BYTES + 1)
        .read_to_end(&mut source)
        .map_err(|error| PluginBuildError::SourceRead {
            path: path.display().to_string(),
            source: error,
        })?;
    if source.len() as u64 > MAX_PLUGIN_SOURCE_BYTES {
        return Err(PluginBuildError::SourceTooLarge {
            path: path.display().to_string(),
            size: source.len() as u64,
            limit: MAX_PLUGIN_SOURCE_BYTES,
        });
    }
    let after = fs::symlink_metadata(path).map_err(|error| PluginBuildError::SourceRead {
        path: path.display().to_string(),
        source: error,
    })?;
    if after.file_type().is_symlink() {
        return Err(PluginBuildError::SourceSymlink {
            path: path.display().to_string(),
        });
    }
    if after.len() != source.len() as u64 || after.modified().ok() != metadata.modified().ok() {
        return Err(PluginBuildError::SourceChanged {
            path: path.display().to_string(),
        });
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_aggregate_aot_before_artifact_retention() {
        let mut total = MAX_PLUGIN_AOT_TOTAL_BYTES - 1;
        assert!(matches!(
            add_aot_size(&mut total, 2),
            Err(zup_bundle::BundleError::PluginAotTooLarge {
                size,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            }) if size == MAX_PLUGIN_AOT_TOTAL_BYTES + 1
        ));
        let mut total = u64::MAX;
        assert!(matches!(
            add_aot_size(&mut total, 1),
            Err(zup_bundle::BundleError::PluginAotTooLarge {
                size: u64::MAX,
                limit: MAX_PLUGIN_AOT_TOTAL_BYTES,
            })
        ));
    }
}
