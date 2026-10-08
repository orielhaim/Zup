use std::path::{Path, PathBuf};

use crate::error::PathError;
use zup_core::{TargetOperatingSystem, TargetTriple};
use zup_platform::TargetPath;

const NAME_MAX: usize = 255;

pub fn linux_target_path(path: &TargetPath) -> Option<&str> {
    (path.target().operating_system() == TargetOperatingSystem::Linux).then_some(path.as_str())
}

pub fn to_host_path(path: &TargetPath) -> Result<PathBuf, PathError> {
    let Some(text) = linux_target_path(path) else {
        return Err(PathError::UnsupportedTarget {
            target: path.target().to_string(),
            path: path.as_str().to_owned(),
        });
    };
    for component in typed_path::Utf8TypedPath::new(text, typed_path::PathType::Unix).components() {
        if component.is_normal() {
            validate_component(component.as_str())?;
        }
    }
    Ok(PathBuf::from(text))
}

pub fn target_path_from_host(path: &Path, target: &TargetTriple) -> Result<TargetPath, PathError> {
    let text = path.to_str().ok_or_else(|| PathError::InvalidComponent {
        component: path.display().to_string(),
        reason: "a path is not text".to_owned(),
    })?;
    if let Some(offender) = windows_shaped(text) {
        return Err(PathError::InvalidComponent {
            component: offender.to_owned(),
            reason: "a Linux path is separated by `/` and has no drive prefix".to_owned(),
        });
    }
    TargetPath::new(target.clone(), text).map_err(PathError::Path)
}

fn windows_shaped(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();

    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Some(&text[..2]);
    }
    text.find('\\').map(|index| &text[index..])
}

fn validate_component(name: &str) -> Result<(), PathError> {
    let invalid = |reason: &str| PathError::InvalidComponent {
        component: name.to_owned(),
        reason: reason.to_owned(),
    };
    if name.len() > NAME_MAX {
        return Err(invalid("a Linux name is at most 255 bytes"));
    }

    if name.contains('\0') {
        return Err(invalid("forbidden character"));
    }

    if name.chars().any(char::is_control) {
        return Err(invalid("control character"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> TargetTriple {
        TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
    }

    fn target_path(text: &str) -> TargetPath {
        TargetPath::new(linux(), text).expect("a canonical Linux path")
    }

    #[test]
    fn refuses_non_linux_target_lowering() {
        let windows = TargetPath::new(
            TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            r"C:\Acme",
        )
        .expect("a canonical Windows path");
        assert!(matches!(
            to_host_path(&windows),
            Err(PathError::UnsupportedTarget { .. })
        ));
        assert_eq!(linux_target_path(&windows), None);
    }

    #[rstest::rstest]
    #[case::root("/", true)]
    #[case::a_plain_path("/opt/acme", true)]
    #[case::a_name_with_spaces("/opt/Acme Studio", true)]
    #[case::a_name_with_punctuation("/opt/acme-1.0 (x86_64):final", true)]
    #[case::a_name_that_windows_forbids("/opt/a:b", true)]
    #[case::a_name_at_the_limit(&format!("/opt/{}", "a".repeat(255)), true)]
    #[case::one_byte_over_the_limit(&format!("/opt/{}", "a".repeat(256)), false)]
    #[case::a_multi_byte_name_at_the_limit(&format!("/opt/{}", "é".repeat(127)), true)]
    #[case::a_control_character("/opt/a\u{1}b", false)]
    fn linux_component_rules(#[case] text: &str, #[case] accepted: bool) {
        let path = target_path(text);
        assert_eq!(
            to_host_path(&path).is_ok(),
            accepted,
            "{text:?} should {}",
            if accepted { "lower" } else { "be refused" }
        );
    }

    #[rstest::rstest]
    #[case::a_drive_letter(r"C:\Users\dev")]
    #[case::a_drive_root(r"C:/Users/dev")]
    #[case::a_lowercase_drive(r"c:\temp")]
    #[case::a_unc_share(r"\\server\share\Acme")]
    #[case::a_device_path(r"\\.\PIPE\acme")]
    #[case::a_trailing_backslash(r"/opt\acme")]
    #[case::a_mixed_separator(r"/opt/acme\bin")]
    #[case::a_backslash_inside_a_name(r"/opt/we\ird")]
    fn a_windows_shaped_path_is_refused_going_the_other_way(#[case] text: &str) {
        let error = target_path_from_host(Path::new(text), &linux())
            .expect_err("a Windows shape is not a Linux path");
        assert!(
            matches!(error, PathError::InvalidComponent { .. }),
            "{text:?} was not refused as a component: {error}"
        );
    }

    #[rstest::rstest]
    #[case::a_plain_path("/opt/acme")]
    #[case::a_root("/")]
    #[case::a_name_with_a_space("/opt/Acme Studio")]
    #[case::a_name_with_a_colon("/opt/a:b")]
    fn a_real_unix_path_lowers_both_ways(#[case] text: &str) {
        let host = PathBuf::from(text);
        let lowered = target_path_from_host(&host, &linux()).expect("a Unix path");
        assert_eq!(lowered.as_str(), text);
        assert_eq!(to_host_path(&lowered).expect("lowers back"), host);
    }

    #[test]
    fn case_is_significant_on_this_target() {
        assert_ne!(target_path("/opt/Acme"), target_path("/opt/acme"));
        assert!(!target_path("/opt/Acme").equivalent(&target_path("/opt/acme")));
    }

    #[test]
    fn a_relative_or_traversing_path_never_reaches_lowering() {
        for text in ["opt/acme", "/opt/../etc", "../etc"] {
            assert!(
                TargetPath::new(linux(), text).is_err(),
                "{text:?} should be refused as a target path"
            );
        }
        assert_eq!(
            TargetPath::new(linux(), "/opt/./acme")
                .expect("a redundant component is canonicalized, not refused")
                .as_str(),
            "/opt/acme"
        );
    }
}
