//! Where an installation keeps the preset content it will present.
//!
//! Two things live here, and they are the same thing: the preset executable and
//! the application-provided assets its settings named. Both are addressed by
//! content, because that is the only addressing that survives the installer that
//! supplied them being deleted, that deduplicates two assets the application
//! configured with the same bytes, and that lets a repair ask for exactly the
//! bytes the ledger recorded.
//!
//! Under the runtime's own directory, and for the same reason the runtime is
//! there: it is installed content in the scope's state root, owned by the same
//! transaction as everything else this installation owns, and removed by the same
//! uninstall. A store anywhere else would need its own ownership record, its own
//! cleanup, and its own recovery, and would eventually disagree with the
//! installation it belongs to.
//!
//! The path of every byte is derived here, from a digest and a logical name
//! alone. The writer that publishes them and the reader that launches them are
//! the same derivation, because a store whose two ends spell its layout
//! differently is a store that works until an update changes the spelling.
//!
//! # Why the executable suffix is an argument
//!
//! Nothing here knows what a native executable is called on any platform. A
//! reserved name ending in `.exe` would be a portable concept naming one
//! platform's convention, and it would be wrong the moment a Linux or macOS
//! runtime resolved one of these paths: it would look for a file no composition
//! for that target ever writes. The suffix therefore arrives from the target the
//! content is for, through [`zup_core::TargetTriple::executable_suffix`], and
//! this module's job is the part that is genuinely the same everywhere.
//!
//! That is also why [`PRESET_SOURCE`] - the payload-relative name a build stage
//! addresses the runtime's own preset by - carries no suffix. A build stage
//! addressing a file by name needs one reserved name, not one per platform, and
//! the *payload* it belongs to is already a single target's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zup_core::{InstalledPreset, RelativePath, Sha256Digest};

/// The payload-relative name that means "the preset executable this image carries".
///
/// The one reserved name in a payload, alongside the maintenance copy. A build
/// stage addresses its files by name, so the runtime's own data needs a name that
/// cannot collide with a file a project happens to ship.
///
/// No executable suffix, deliberately: the name is a reserved *slot* in a payload
/// rather than a file name on a filesystem, and a slot that ended in one
/// platform's suffix could not be the same slot on another.
pub const PRESET_SOURCE: &str = "__zup_preset__";

/// The payload-relative prefix that means "an application-provided preset asset".
pub const ASSET_SOURCE_PREFIX: &str = "__zup_preset_asset__";

/// The directory an installation's preset content is kept in.
const PRESET_DIRECTORY: &str = "ui";

/// Where an installation keeps one preset executable.
///
/// `executable_suffix` is the target's own, which is empty for every platform that
/// has no such convention and `.exe` for the one that does.
pub fn preset_path(
    runtime_directory: &Path,
    digest: &Sha256Digest,
    executable_suffix: &str,
) -> PathBuf {
    runtime_directory
        .join(PRESET_DIRECTORY)
        .join("preset")
        .join(format!("{}{executable_suffix}", digest.to_hex()))
}

/// Where an installation keeps one application-provided asset.
///
/// The logical name is a directory and the digest is the file. Naming it this way
/// means the path a preset is handed still says what the application called the
/// asset, while the bytes under it are addressed by content - so the same logo for
/// two components is one file, and a changed logo is a new file rather than an
/// overwrite that a running preset could observe half-applied.
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

/// The payload-relative name for one asset's bytes.
pub fn asset_source_name(name: &str) -> RelativePath {
    RelativePath::from_components([ASSET_SOURCE_PREFIX, name])
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
/// Assets first, then the executable, for the same reason the composed path does
/// it: a missing asset is a specific thing the application configured, and an
/// installation missing both would otherwise be reported as a missing executable,
/// which is the same mistake twice with the less useful half kept.
///
/// `asset_text` turns a resolved path into the text a preset is handed. It is an
/// argument rather than a call because *how* a path is spelled for display is a
/// property of the host - Windows hands back extended-length paths that are an
/// implementation detail of the call, and no other host does - and this module
/// has no business knowing which host it is on.
pub fn resolve(
    runtime_directory: &Path,
    ui: &InstalledPreset,
    executable_suffix: &str,
    asset_text: impl Fn(&Path) -> String,
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
        assets.insert(asset.name.to_string(), asset_text(&path));
    }
    let executable = preset_path(runtime_directory, &ui.executable, executable_suffix);
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// The reserved name is a slot, not a file name. A slot carrying one
    /// platform's suffix cannot be the same slot on another, and this is the rule
    /// that would otherwise be broken by someone "fixing" the name to `.exe`.
    #[test]
    fn the_reserved_slot_name_carries_no_executable_suffix() {
        assert!(
            !PRESET_SOURCE.ends_with(".exe"),
            "a reserved payload name is a slot, not a file name"
        );
        assert!(
            !ASSET_SOURCE_PREFIX.ends_with(".exe"),
            "and so is the asset prefix"
        );
    }

    /// The suffix is the target's, so two targets resolve two different file names
    /// from the same digest - and the Linux one has no suffix at all, which is the
    /// case a hard-coded `.exe` gets wrong.
    #[rstest]
    #[case::windows(".exe", true)]
    #[case::linux("", false)]
    fn the_executable_file_name_follows_the_targets_own_suffix(
        #[case] suffix: &str,
        #[case] ends_with_exe: bool,
    ) {
        let directory = Path::new("/var/lib/zup/maintenance");
        let digest = Sha256Digest::from_bytes([9; 32]);
        let path = preset_path(directory, &digest, suffix);
        assert_eq!(
            path.to_string_lossy().ends_with(".exe"),
            ends_with_exe,
            "{path:?}"
        );
        assert!(path.to_string_lossy().contains(&digest.to_hex()));
        assert!(path.starts_with(directory.join(PRESET_DIRECTORY)));
    }

    /// An asset's logical name is spelled a segment at a time, so a manifest
    /// writing `branding/logo.svg` produces a real directory rather than one
    /// component containing slashes - which on Windows would open, and would be a
    /// different string from the one the ledger compares against.
    #[test]
    fn an_assets_logical_name_becomes_real_segments() {
        let directory = Path::new("/var/lib/zup/maintenance/1.0.0");
        let digest = Sha256Digest::from_bytes([3; 32]);
        let path = asset_path(directory, "branding/logo.svg", &digest);
        let relative = path
            .strip_prefix(directory)
            .expect("under the runtime directory");
        let segments = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            segments,
            vec![
                "ui".to_owned(),
                "assets".to_owned(),
                "branding".to_owned(),
                "logo.svg".to_owned(),
                digest.to_hex(),
            ],
            "each segment of the logical name is its own directory entry"
        );
    }

    /// A digest that does not match the recorded one is a refusal, not a warning:
    /// the caller is about to load these bytes, and a path that might or might not
    /// be right is not something to hand a program.
    #[test]
    fn content_is_proved_before_a_path_is_handed_over() {
        let root = tempfile::tempdir().expect("a temp directory");
        let bytes = b"preset";
        let ui = InstalledPreset {
            executable: zup_core::hash_bytes(bytes),
            preset: zup_core::PresetRuntime {
                name: zup_core::NonEmptyString::new("acme").expect("a name"),
                version: semver::Version::new(1, 0, 0),
                protocol: 1,
                required_capabilities: Vec::new(),
                settings: serde_json::Value::Null,
                assets: Vec::new(),
            },
        };
        let error = resolve(root.path(), &ui, "", |path| path.display().to_string())
            .expect_err("nothing is installed yet");
        assert!(
            matches!(error, PresetContentError::PresetMissing(_)),
            "{error}"
        );

        // With the right bytes at the right path, the same call succeeds.
        let expected = preset_path(root.path(), &ui.executable, "");
        std::fs::create_dir_all(expected.parent().expect("a parent")).expect("create");
        std::fs::write(&expected, bytes).expect("write");
        let resolved = resolve(root.path(), &ui, "", |path| path.display().to_string())
            .expect("a proved preset resolves");
        assert_eq!(resolved.executable, expected);

        // And bytes that do not hash to the recorded digest are a refusal naming
        // both digests, because the caller is about to load them.
        std::fs::write(&expected, b"tampered").expect("write");
        let error = resolve(root.path(), &ui, "", |path| path.display().to_string())
            .expect_err("damaged content is refused");
        assert!(
            matches!(error, PresetContentError::PresetDamaged { .. }),
            "{error}"
        );
    }
}
