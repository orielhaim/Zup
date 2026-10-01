//! Lowering a [`TargetPath`] onto this Windows host.
//!
//! The lexical half of a target path is already settled: absolute, canonical,
//! free of device namespaces. What is left is what Windows will still refuse to
//! name a file with, which `typed-path` does not model because it is a rule
//! about a filesystem rather than about a path's spelling.

use std::path::{Path, PathBuf};

use thiserror::Error;
use typed_path::{
    Utf8Component, Utf8WindowsComponent, constants::windows::RESERVED_DEVICE_NAMES_STR,
};
use zup_core::{TargetOperatingSystem, TargetTriple};
use zup_platform::{TargetPath, TargetPathError};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetPathValidationError {
    #[error("target path targets `{target}`, not Windows")]
    UnsupportedTarget { target: String },

    #[error("Windows path component `{component}` is invalid: {reason}")]
    InvalidComponent { component: String, reason: String },
}

#[derive(Debug, Error)]
pub enum TargetPathLoweringError {
    #[error("target path `{path}` targets `{target}`, not Windows")]
    UnsupportedTarget { target: String, path: String },

    #[error(transparent)]
    InvalidPath(#[from] TargetPathValidationError),
}

pub fn validate_windows_target_path(path: &TargetPath) -> Result<(), TargetPathValidationError> {
    let Some(windows) = path.as_windows() else {
        return Err(TargetPathValidationError::UnsupportedTarget {
            target: path.target().to_string(),
        });
    };
    // The prefix and the root are structural, so only the names are judged.
    for component in windows.components() {
        if let Utf8WindowsComponent::Normal(name) = component {
            validate_component(component, name)?;
        }
    }
    Ok(())
}

/// Windows path identity: the canonical spelling, case-folded.
pub fn windows_target_path_identity(path: &TargetPath) -> String {
    path.as_str().to_lowercase()
}

pub fn to_host_path(path: &TargetPath) -> Result<PathBuf, TargetPathLoweringError> {
    if path.target().operating_system() != TargetOperatingSystem::Windows {
        return Err(TargetPathLoweringError::UnsupportedTarget {
            target: path.target().to_string(),
            path: path.as_str().to_owned(),
        });
    }
    validate_windows_target_path(path)?;
    Ok(PathBuf::from(path.as_str()))
}

pub(crate) fn host_path(path: &TargetPath) -> PathBuf {
    to_host_path(path).expect("Windows backend received an invalid target path")
}

pub(crate) fn target_path_from_host(
    path: &Path,
    target: &TargetTriple,
) -> Result<TargetPath, TargetPathError> {
    TargetPath::new(target.clone(), crate::machine_state::plain_path_text(path))
}

/// Windows' own filename rules, applied to one component.
///
/// A reserved device name, a separator, an illegal character, or a trailing dot
/// or space makes a component unrepresentable, and a path is only refused if
/// one of its components is.
fn validate_component(
    component: Utf8WindowsComponent<'_>,
    name: &str,
) -> Result<(), TargetPathValidationError> {
    let invalid = |reason: &str| TargetPathValidationError::InvalidComponent {
        component: name.to_owned(),
        reason: reason.to_owned(),
    };

    if name.ends_with(' ') || name.ends_with('.') {
        return Err(invalid("trailing spaces and dots are not allowed"));
    }
    // `is_valid` is the filename-character rule: a separator or a character
    // Windows reserves inside a name.
    if !component.is_valid() || name.chars().any(char::is_control) {
        return Err(invalid("forbidden or control character"));
    }
    // `nul.txt` is still the `NUL` device, so the name that matters is the stem.
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED_DEVICE_NAMES_STR
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(invalid("reserved device name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::TargetTriple;

    fn target_path(path: &str) -> TargetPath {
        TargetPath::new(TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(), path).unwrap()
    }

    #[test]
    fn a_canonical_windows_path_is_already_a_host_path() {
        let path = target_path(r"C:/Program Files/Acme");
        assert_eq!(
            to_host_path(&path).unwrap().to_string_lossy(),
            r"C:\Program Files\Acme"
        );
    }

    #[test]
    fn refuses_non_windows_target_lowering() {
        let path = TargetPath::new(
            TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap(),
            "/opt/acme",
        )
        .unwrap();
        assert!(matches!(
            to_host_path(&path),
            Err(TargetPathLoweringError::UnsupportedTarget { .. })
        ));
    }

    #[test]
    fn every_component_is_validated_without_host_path_semantics() {
        for (path, accepted) in [
            (r"C:\CON", false),
            (r"C:\nul.txt", false),
            (r"C:\PRN.txt", false),
            (r"C:\AUX", false),
            (r"C:\COM1.dat", false),
            (r"C:\COM9.log", false),
            (r"C:\LPT1", false),
            (r"C:\LPT9.dat", false),
            (r"C:\file.", false),
            (r"C:\file ", false),
            (r"C:\a:b", false),
            (r"C:\a?b", false),
            (r"C:\a*b", false),
            (r"C:\a<b", false),
            (r"C:\a>b", false),
            (r"C:\a|b", false),
            ("C:\\file\u{0001}.txt", false),
            (r"C:\", true),
            (r"C:\Program Files\Acme\app.exe", true),
            (r"\\server\share\Acme\app.exe", true),
        ] {
            assert_eq!(
                validate_windows_target_path(&target_path(path)).is_ok(),
                accepted,
                "{path:?}"
            );
        }
    }

    #[test]
    fn device_paths_are_rejected_at_the_target_path_boundary() {
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        for path in [
            r"\\.\PIPE\device",
            r"\\?\C:\Windows",
            r"\\?\UNC\server\share",
            r"\\??\C:\Windows",
        ] {
            assert!(TargetPath::new(&target, path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn identity_ignores_case_and_separator_spelling() {
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        assert_eq!(
            windows_target_path_identity(&target_path(r"C:\Apps\Acme")),
            windows_target_path_identity(&TargetPath::new(&target, r"c:/apps/acme").unwrap()),
        );
    }

    #[test]
    fn strips_extended_host_prefix_before_target_parsing() {
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let path = target_path_from_host(Path::new(r"\\?\C:\Program Files\Acme"), &target).unwrap();
        assert_eq!(path.as_str(), r"C:\Program Files\Acme");
    }
}
