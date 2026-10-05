//! Finding this machine's preset executable, and the files the application gave
//! it.
//!
//! Everything after that - the endpoint, the handshake, publishing a snapshot,
//! reading an action, and ending the child - is in `zup_preset_host::process`,
//! because a development environment does exactly the same and a host with two
//! launch paths would be a host whose behaviour could not be established from
//! either.
//!
//! What stays here is the question the rest cannot answer: on *this* machine, the
//! preset's bytes are in one of two places. A fresh install reads the installer
//! image it was started from, which is the only place the bytes exist before
//! anything has been committed. Everything after that reads the installation's
//! own durable state, which is the only place the bytes exist once the installer
//! is gone.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use zup_core::PresetRuntime;
use zup_preset_protocol::Capabilities;

/// Why no window appeared.
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    #[error("this installation has no window to show: {0}")]
    Missing(String),
    #[error("this host cannot present the preset: {0}")]
    Incompatible(String),
    #[error("the installed UI content cannot be used: {0}")]
    Integrity(#[from] zup_bundle::PresetContentError),
    #[error("the preset could not be prepared for launch: {0}")]
    Prepare(#[source] io::Error),
}

/// Where this launch's preset comes from.
///
/// Two sources and one launch path. Nothing downstream of here knows which of the
/// two it was: the compatibility check, the settings, the asset table and the
/// launch are the same work either way, and a host with a different path per
/// source would be a host whose maintenance behaviour could not be established
/// from its install behaviour.
pub enum Source<'a> {
    /// The installer image this process is running from, before any commit.
    Composed {
        executable: &'a Path,
        preset: &'a PresetRuntime,
        bundle: &'a zup_windows::EmbeddedBundle,
    },
    /// What an installed application owns.
    Installed {
        /// The installation's maintenance directory, where its content is kept.
        directory: &'a Path,
        runtime: &'a zup_core::InstalledPreset,
        /// The target this installation installed for, which decides the file name
        /// its content was persisted under.
        target: &'a zup_core::TargetTriple,
    },
}

/// The preset this installer was composed with, and the assets it was given.
///
/// One value, because a preset cannot draw without both and a host that
/// materialized one without the other would be a host that starts a window which
/// cannot load what the application configured.
#[derive(Debug)]
pub struct Composed {
    /// The executable to launch.
    pub executable: PathBuf,
    /// What to tell the preset, once its executable is running.
    pub configuration: zup_preset_protocol::Configuration,
}

/// Materialize the preset executable and its assets, and check this launch can
/// present them.
pub fn materialize(
    source: &Source<'_>,
    capabilities: &Capabilities,
) -> Result<Composed, PresetError> {
    let preset = match source {
        Source::Composed { preset, .. } => *preset,
        Source::Installed { runtime, .. } => &runtime.preset,
    };
    zup_preset_host::process::check_presentable(preset, capabilities)
        .map_err(PresetError::Incompatible)?;

    let (executable, assets) = match source {
        Source::Installed {
            directory,
            runtime,
            target,
        } => {
            // The suffix is the installation's target's, so the path this resolves
            // is the one the installation actually wrote under its own naming rule.
            let resolved = zup_bundle::resolve_preset_content(
                directory,
                runtime,
                target.executable_suffix(),
                // How a resolved path is spelled for display is the host's business:
                // Windows hands back extended-length paths that are an
                // implementation detail of the call, and no other host does.
                zup_windows::plain_path_text,
            )?;
            (resolved.executable, resolved.assets)
        }
        Source::Composed {
            executable, bundle, ..
        } => {
            // The assets first, then the executable. A missing asset is a
            // specific thing this image can describe; a missing executable in an
            // image that is also missing its assets is the same mistake twice,
            // and the more useful of the two names what the application
            // configured.
            let mut assets = BTreeMap::new();
            for asset in &preset.assets {
                let (_, bytes) = bundle.ui_asset(asset.name.as_str()).map_err(|error| {
                    PresetError::Missing(format!("the asset `{}`: {error}", asset.name))
                })?;
                let path = staging_directory(executable).join(asset.name.as_str());
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| PresetError::Prepare(io::Error::other(error)))?;
                }
                zup_windows::write_durable(&path, &bytes)
                    .map_err(|error| PresetError::Prepare(io::Error::other(error.to_string())))?;
                assets.insert(asset.name.to_string(), zup_windows::plain_path_text(&path));
            }
            (locate(executable)?, assets)
        }
    };

    Ok(Composed {
        executable,
        configuration: zup_preset_protocol::Configuration {
            settings: preset.settings.clone(),
            assets,
        },
    })
}

/// Where a not-yet-committed install writes what the application configured.
///
/// Beside the image, and for the length of one session. None of this is installed
/// state: an installation that has committed keeps the same assets under its own
/// maintenance directory, content-addressed and owned.
fn staging_directory(executable: &Path) -> PathBuf {
    executable
        .parent()
        .unwrap_or(Path::new("."))
        .join("preset-assets")
}

/// Write the installer image's own embedded preset out beside itself.
///
/// Verified by the bundle's own digests on the way in and written the same way
/// the rest of a maintenance payload is, because a preset that was not proved is
/// a program nobody has reviewed.
fn locate(executable: &Path) -> Result<PathBuf, PresetError> {
    let composed =
        executable
            .parent()
            .unwrap_or(Path::new("."))
            .join(zup_windows::preset_executable_name(
                std::env::consts::EXE_SUFFIX,
            ));
    if composed.is_file() {
        return Ok(composed);
    }
    let bundle = crate::package::open_bundle(executable)
        .map_err(|error| PresetError::Prepare(io::Error::other(error.to_string())))?;
    let bytes = bundle.preset().ok_or(PresetError::Missing(
        "this installer was composed without a preset".into(),
    ))?;
    zup_windows::write_durable(&composed, bytes)
        .map_err(|error| PresetError::Prepare(io::Error::other(error.to_string())))?;
    Ok(composed)
}

// The session itself. Re-exported rather than reimplemented, so a caller that
// already names this module keeps naming it.
pub use zup_preset_host::process::{PresetProcess, PresetReader, SessionError, launch};
