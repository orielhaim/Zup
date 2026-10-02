//! Compile the icon a target will install.
//!
//! Each target asks for its own outputs. A Windows plan carries an ICO, a
//! Linux plan carries a hicolor tree, and neither carries the other's files.

use std::fs;
use std::path::Path;

use zup_assets::{IconCache, IconFormat, IconSource, IconTarget};
use zup_core::{CompiledIcon, IconRole, Installer, TargetIcons, TargetOperatingSystem, hash_bytes};
use zup_manifest::IconConfig;
use zup_platform::SourceFilePolicy;

use crate::error::BuildError;
use crate::materialize::resolve_project_source;

const MAX_ICON_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn compile_icons(
    project_root: &Path,
    cache: &IconCache,
    icon: Option<&IconConfig>,
    installer: &Installer,
    policy: &dyn SourceFilePolicy,
) -> Result<TargetIcons, BuildError> {
    let Some(target) = icon_target(installer) else {
        return Ok(TargetIcons::default());
    };
    let (label, bytes, format, padding, fallback) = match icon {
        Some(icon) => {
            let relative = icon
                .source
                .to_relative()
                .map_err(|error| BuildError::Icon {
                    path: icon.source.to_string(),
                    message: error.to_string(),
                })?;
            let (path, _, _) =
                resolve_project_source(project_root, &relative, "icon", MAX_ICON_BYTES, policy)
                    .map_err(|error| match error {
                        BuildError::Io { path, source }
                            if source.kind() == std::io::ErrorKind::NotFound =>
                        {
                            BuildError::IconMissing {
                                path: path.display().to_string(),
                            }
                        }
                        other => other,
                    })?;
            let bytes = fs::read(&path).map_err(|source| BuildError::Io {
                path: path.clone(),
                source,
            })?;
            let format =
                zup_assets::format_of(icon.source.as_str()).map_err(|error| BuildError::Icon {
                    path: icon.source.to_string(),
                    message: error.to_string(),
                })?;
            (
                icon.source.to_string(),
                bytes,
                format,
                icon.padding(),
                false,
            )
        }
        None => (
            "the built-in Zup icon".to_owned(),
            zup_assets::ZUP_ICON_SVG.to_vec(),
            IconFormat::Svg,
            0.0,
            true,
        ),
    };
    let compiled = cache
        .compile(
            &IconSource {
                label: &label,
                bytes: &bytes,
                format,
                padding,
            },
            &[target],
        )
        .map_err(|error| BuildError::Icon {
            path: label.clone(),
            message: error.to_string(),
        })?;
    let mut artifacts = compiled
        .artifacts
        .into_iter()
        .map(|artifact| CompiledIcon {
            role: match artifact.role {
                zup_assets::IconRole::Windows => IconRole::Windows,
                zup_assets::IconRole::MacOs => IconRole::MacOs,
                zup_assets::IconRole::LinuxSvg => IconRole::LinuxSvg,
                zup_assets::IconRole::LinuxPng { size } => IconRole::LinuxPng { size },
                zup_assets::IconRole::Png { size } => IconRole::Png { size },
            },
            name: artifact.name,
            size: artifact.bytes.len() as u64,
            sha256: hash_bytes(&artifact.bytes),
            executable_images: artifact
                .executable
                .as_ref()
                .map(|icon| icon.images.clone())
                .unwrap_or_default(),
            executable_group: artifact
                .executable
                .map(|icon| icon.group)
                .unwrap_or_default(),
            // An in-memory compile has no file behind the icon, and naming one
            // would point a consumer at nothing.
            source: (!artifact.path.as_os_str().is_empty()).then_some(artifact.path),
        })
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(TargetIcons {
        artifacts,
        warnings: compiled.warnings,
        fallback,
    })
}

fn icon_target(installer: &Installer) -> Option<IconTarget> {
    match installer.target.operating_system() {
        TargetOperatingSystem::Windows => Some(IconTarget::Windows),
        TargetOperatingSystem::Darwin(_) | TargetOperatingSystem::MacOSX { .. } => {
            Some(IconTarget::MacOs)
        }
        TargetOperatingSystem::Linux => Some(IconTarget::Linux {
            app_id: installer.app.id.to_string(),
        }),
        _ => None,
    }
}
