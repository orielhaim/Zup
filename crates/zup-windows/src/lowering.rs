use std::path::{Path, PathBuf};

use thiserror::Error;
use typed_path::{Utf8Component, Utf8WindowsComponent};

use crate::shortcut_name::is_reserved_device_stem;
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

    for component in windows.components() {
        if let Utf8WindowsComponent::Normal(name) = component {
            validate_component(component, name)?;
        }
    }
    Ok(())
}

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

    if !component.is_valid() || name.chars().any(char::is_control) {
        return Err(invalid("forbidden or control character"));
    }

    let stem = name.split('.').next().unwrap_or(name);
    if is_reserved_device_stem(stem) {
        return Err(invalid("reserved device name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
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

    #[rstest]
    #[case(r"C:\CON", false)]
    #[case(r"C:\nul.txt", false)]
    #[case(r"C:\PRN.txt", false)]
    #[case(r"C:\AUX", false)]
    #[case(r"C:\COM1.dat", false)]
    #[case(r"C:\COM9.log", false)]
    #[case(r"C:\LPT1", false)]
    #[case(r"C:\LPT9.dat", false)]
    #[case(r"C:\file.", false)]
    #[case(r"C:\file ", false)]
    #[case(r"C:\a:b", false)]
    #[case(r"C:\a?b", false)]
    #[case(r"C:\a*b", false)]
    #[case(r"C:\a<b", false)]
    #[case(r"C:\a>b", false)]
    #[case(r"C:\a|b", false)]
    #[case("C:\\file\u{0001}.txt", false)]
    #[case(r"C:\", true)]
    #[case(r"C:\Program Files\Acme\app.exe", true)]
    #[case(r"\\server\share\Acme\app.exe", true)]
    fn every_component_is_validated_without_host_path_semantics(
        #[case] path: &str,
        #[case] accepted: bool,
    ) {
        assert_eq!(
            validate_windows_target_path(&target_path(path)).is_ok(),
            accepted
        );
    }

    #[rstest]
    #[case(r"\\.\PIPE\device")]
    #[case(r"\\?\C:\Windows")]
    #[case(r"\\?\UNC\server\share")]
    #[case(r"\\??\C:\Windows")]
    fn device_paths_are_rejected_at_the_target_path_boundary(#[case] path: &str) {
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        assert!(TargetPath::new(&target, path).is_err());
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
