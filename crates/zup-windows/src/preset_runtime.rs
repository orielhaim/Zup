//! Where an installation keeps the preset runtime it will present.
//!
//! Two things live here, and they are the same thing: the preset executable and
//! the application-provided assets its settings named. Both are addressed by
//! content, because that is the only addressing that survives the installer that
//! supplied them being deleted, that deduplicates two assets the application
//! configured with the same bytes, and that lets a repair ask for exactly the
//! bytes the ledger recorded.
//!
//! Under the maintenance runtime's own directory, and for the same reason the
//! runtime is there: it is installed content in the scope's state root, owned by
//! the same transaction as everything else this installation owns, and removed
//! by the same uninstall. A store anywhere else would need its own ownership
//! record, its own cleanup, and its own recovery, and would eventually disagree
//! with the installation it belongs to.
//!
//! The path of every byte is derived here, from a digest and a logical name
//! alone. The writer that publishes them and the reader that launches them are
//! the same derivation, because a store whose two ends spell its layout
//! differently is a store that works until an update changes the spelling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{InstalledPreset, Sha256Digest};

use crate::machine_state;

/// The payload-relative name that means "the preset executable this image carries".
///
/// The one reserved name in a payload, alongside the maintenance copy. A build
/// stage addresses its files by name, so the runtime's own data needs a name
/// that cannot collide with a file a project happens to ship.
pub const PRESET_SOURCE: &str = "__zup_preset__.exe";

/// The payload-relative prefix that means "an application-provided preset asset".
pub const ASSET_SOURCE_PREFIX: &str = "__zup_preset_asset__";

/// The directory an installation's preset content is kept in.
const PRESET_DIRECTORY: &str = "ui";

/// Where an installation keeps one preset executable.
pub fn preset_path(runtime_directory: &Path, digest: &Sha256Digest) -> PathBuf {
    runtime_directory
        .join(PRESET_DIRECTORY)
        .join("preset")
        .join(preset_file_name(digest))
}

/// Where an installation keeps one application-provided asset.
///
/// The logical name is a directory and the digest is the file. Naming it this
/// way means the path a preset is handed still says what the application called
/// the asset, while the bytes under it are addressed by content - so the same
/// logo for two components is one file, and a changed logo is a new file rather
/// than an overwrite that a running preset could observe half-applied.
///
/// The name is written a segment at a time rather than joined whole. A preset
/// author writes `branding/logo.svg`, and joining that as one string produces a
/// path with a forward slash in the middle on Windows: it would open, and it
/// would be a different string from the one everything that compares installed
/// paths - the ledger, the installer - is holding.
pub fn asset_path(runtime_directory: &Path, name: &str, digest: &Sha256Digest) -> PathBuf {
    let mut path = runtime_directory.join(PRESET_DIRECTORY).join("assets");
    for segment in name.split('/').filter(|segment| !segment.is_empty()) {
        path.push(segment);
    }
    path.push(digest.to_hex());
    path
}

/// The preset executable's file name.
///
/// A suffix, because Windows will not launch a file without one, and the
/// content address is the rest.
fn preset_file_name(digest: &Sha256Digest) -> String {
    format!("{}{}", digest.to_hex(), std::env::consts::EXE_SUFFIX)
}

/// The payload-relative name for one asset's bytes.
pub fn asset_source_name(name: &str) -> zup_core::RelativePath {
    zup_core::RelativePath::from_components([ASSET_SOURCE_PREFIX, name])
        .expect("a reserved asset source name is always relative")
}

/// Why an installed application's preset content could not be used.
#[derive(Debug, thiserror::Error)]
pub enum PresetContentError {
    #[error("the installed preset runtime names preset content that is not on this machine: {0}")]
    PresetMissing(String),
    #[error(
        "the installed preset executable is {size} bytes and hashes to {found}, not the {expected} this installation recorded"
    )]
    PresetDamaged {
        size: u64,
        found: Sha256Digest,
        expected: Sha256Digest,
    },
    #[error("the asset `{name}` this installation recorded is not on this machine")]
    AssetMissing { name: String },
    #[error(
        "the asset `{name}` is {size} bytes and hashes to {found}, not the {expected} this installation recorded"
    )]
    AssetDamaged {
        name: String,
        size: u64,
        found: Sha256Digest,
        expected: Sha256Digest,
    },
}

/// The verified executable and asset paths of an installed preset runtime.
///
/// Every byte proved against the digest the installation recorded, before the
/// caller is told where it is. A path that might or might not be right is not
/// something to hand a program that is about to load it.
#[derive(Debug)]
pub struct Resolved {
    pub executable: PathBuf,
    pub assets: BTreeMap<String, String>,
}

/// Locate an installed preset runtime's content, proving each byte.
///
/// Assets first, then the executable, for the same reason the composed path
/// does it: a missing asset is a specific thing the application configured, and
/// an installation missing both would otherwise be reported as a missing
/// executable, which is the same mistake twice with the less useful half kept.
pub fn resolve(
    runtime_directory: &Path,
    ui: &InstalledPreset,
) -> Result<Resolved, PresetContentError> {
    let mut assets = BTreeMap::new();
    for asset in &ui.preset.assets {
        let path = asset_path(runtime_directory, asset.name.as_str(), &asset.sha256);
        match digest_of(&path) {
            Ok((_, found)) if found == asset.sha256 => {}
            Ok((size, found)) => {
                return Err(PresetContentError::AssetDamaged {
                    name: asset.name.to_string(),
                    size,
                    found,
                    expected: asset.sha256,
                });
            }
            Err(()) => {
                return Err(PresetContentError::AssetMissing {
                    name: asset.name.to_string(),
                });
            }
        }
        assets.insert(
            asset.name.to_string(),
            machine_state::plain_path_text(&path),
        );
    }
    let executable = preset_path(runtime_directory, &ui.executable);
    match digest_of(&executable) {
        Ok((_, found)) if found == ui.executable => {}
        Ok((size, found)) => {
            return Err(PresetContentError::PresetDamaged {
                size,
                found,
                expected: ui.executable,
            });
        }
        Err(()) => {
            return Err(PresetContentError::PresetMissing(
                executable.display().to_string(),
            ));
        }
    }
    Ok(Resolved { executable, assets })
}

/// A file's length and digest, or nothing if it cannot be read whole.
fn digest_of(path: &Path) -> Result<(u64, Sha256Digest), ()> {
    let mut file = std::fs::File::open(path).map_err(|_| ())?;
    zup_core::hash_reader(&mut file).map_err(|_| ())
}

/// Whether a destination is one of an installation's preset content files.
///
/// Asked of the ledger rather than assumed, so that retiring a window's bytes is
/// decided by where they are and not by a marker that could go stale. Scoped to
/// the application's maintenance root rather than to one version's directory,
/// because the question is asked of every generation an installation has owned.
pub fn is_content_path(maintenance_root: &Path, destination: &str) -> bool {
    let Ok(relative) = Path::new(destination).strip_prefix(maintenance_root) else {
        return false;
    };
    // One version segment, then the content directory, then at least one more.
    let mut components = relative.components();
    components.next();
    components
        .next()
        .is_some_and(|component| component.as_os_str() == PRESET_DIRECTORY)
        && components.next().is_some()
}
