use std::path::{Path, PathBuf};

use thiserror::Error;
use zup_core::{TargetOperatingSystem, TargetTriple};
use zup_platform::{TargetPath, TargetPathError};

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
];

const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetPathValidationError {
    #[error("target path targets `{target}`, not Windows")]
    UnsupportedTarget { target: String },

    #[error("Windows device path `{path}` is not supported")]
    DevicePath { path: String },

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
    if path.target().operating_system() != TargetOperatingSystem::Windows {
        return Err(TargetPathValidationError::UnsupportedTarget {
            target: path.target().to_string(),
        });
    }

    let lexical = path.as_str();
    if lexical.starts_with(r"\\?\") || lexical.starts_with(r"\\.") || lexical.starts_with(r"\\??\")
    {
        return Err(TargetPathValidationError::DevicePath {
            path: lexical.to_owned(),
        });
    }

    if let Some(unc) = lexical.strip_prefix(r"\\") {
        for component in unc.split('\\') {
            if component.is_empty() {
                return Err(TargetPathValidationError::InvalidComponent {
                    component: component.to_owned(),
                    reason: "empty path component".to_owned(),
                });
            }
            validate_component(component)?;
        }
        return Ok(());
    }

    let Some(suffix) = lexical.get(3..) else {
        return Err(TargetPathValidationError::InvalidComponent {
            component: lexical.to_owned(),
            reason: "missing Windows drive root".to_owned(),
        });
    };
    if suffix.is_empty() {
        return Ok(());
    }
    for component in suffix.split('\\') {
        if component.is_empty() {
            return Err(TargetPathValidationError::InvalidComponent {
                component: component.to_owned(),
                reason: "empty path component".to_owned(),
            });
        }
        validate_component(component)?;
    }

    Ok(())
}

pub fn windows_target_path_identity(path: &TargetPath) -> String {
    path.as_str().replace('/', "\\").to_lowercase()
}

pub fn to_host_path(path: &TargetPath) -> Result<PathBuf, TargetPathLoweringError> {
    if path.target().operating_system() != TargetOperatingSystem::Windows {
        return Err(TargetPathLoweringError::UnsupportedTarget {
            target: path.target().to_string(),
            path: path.as_str().to_owned(),
        });
    }
    validate_windows_target_path(path)?;
    let lexical = path.as_str().replace('/', "\\");
    Ok(PathBuf::from(lexical))
}

pub(crate) fn host_path(path: &TargetPath) -> PathBuf {
    to_host_path(path).expect("Windows backend received an invalid target path")
}

pub(crate) fn target_path_from_host(
    path: &Path,
    target: &TargetTriple,
) -> Result<TargetPath, TargetPathError> {
    TargetPath::new(target.clone(), host_path_text(path))
}

fn host_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(path) = text.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        text.into_owned()
    }
}

fn validate_component(component: &str) -> Result<(), TargetPathValidationError> {
    if component.ends_with(' ') || component.ends_with('.') {
        return Err(TargetPathValidationError::InvalidComponent {
            component: component.to_owned(),
            reason: "trailing spaces and dots are not allowed".to_owned(),
        });
    }

    if component
        .chars()
        .any(|character| FORBIDDEN_CHARS.contains(&character) || character.is_control())
    {
        return Err(TargetPathValidationError::InvalidComponent {
            component: component.to_owned(),
            reason: "forbidden or control character".to_owned(),
        });
    }

    let stem = component.split('.').next().unwrap_or(component);
    if RESERVED_NAMES
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
    {
        return Err(TargetPathValidationError::InvalidComponent {
            component: component.to_owned(),
            reason: "reserved device name".to_owned(),
        });
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
    fn lowers_windows_lexical_separators_without_changing_case() {
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
    fn validates_every_component_without_host_path_semantics() {
        for path in [
            r"C:\CON",
            r"C:\nul.txt",
            r"C:\PRN.txt",
            r"C:\AUX",
            r"C:\COM1.dat",
            r"C:\COM9.log",
            r"C:\LPT1",
            r"C:\LPT9.dat",
            r"C:\file.",
            r"C:\file ",
            r"C:\a:b",
            r"C:\a?b",
            r"C:\a*b",
            r"C:\a<b",
            r"C:\a>b",
            r"C:\a|b",
            "C:\\file\u{0001}.txt",
        ] {
            assert!(
                validate_windows_target_path(&target_path(path)).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn accepts_valid_drive_and_unc_components() {
        for path in [
            r"C:\",
            r"C:\Program Files\Acme\app.exe",
            r"\\server\share\Acme\app.exe",
        ] {
            assert!(
                validate_windows_target_path(&target_path(path)).is_ok(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn rejects_device_paths_at_the_target_path_boundary() {
        for path in [r"\\.\PIPE\device", r"\\?\C:\Windows"] {
            let result =
                TargetPath::new(TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(), path);
            assert!(result.is_err(), "{path:?}");
        }
    }

    #[test]
    fn strips_extended_host_prefix_before_target_parsing() {
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let path = target_path_from_host(Path::new(r"\\?\C:\Program Files\Acme"), &target).unwrap();
        assert_eq!(path.as_str(), r"C:\Program Files\Acme");
    }

    #[test]
    fn preserves_the_requested_target_when_reconstructing_paths() {
        let target = TargetTriple::parse("arm64-pc-windows-msvc").unwrap();
        let path = target_path_from_host(Path::new(r"C:\Program Files\Acme"), &target).unwrap();
        assert_eq!(path.target(), &target);
    }
}
