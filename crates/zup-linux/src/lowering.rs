//! Lowering a [`TargetPath`] onto this Linux host.
//!
//! A target path is already bound to the target it names and already canonical:
//! absolute, free of template variables, free of `..`. What is left is what Linux
//! itself will still refuse to name a file with, which `typed-path` does not model
//! because it is a rule about a filesystem rather than about a spelling.
//!
//! The important property here is that the refusal is *closed*. A Linux target
//! path is Unix-shaped and case-sensitive, and a path that arrives carrying a
//! Windows drive letter or a backslash separator is not a path this backend may
//! quietly reinterpret: it is a path that names somewhere else on the machine, and
//! writing there would be exactly the substitution the caller asked to be
//! prevented. So `to_host_path` refuses a path whose target is not Linux, and the
//! component rules below never accept a character Linux forbids.
//!
//! [`TargetPath`]: zup_platform::TargetPath

use std::path::{Path, PathBuf};

use thiserror::Error;
use zup_core::{TargetOperatingSystem, TargetTriple};
use zup_platform::{TargetPath, TargetPathError};

/// The longest single path component Linux accepts.
///
/// `NAME_MAX` is 255 on every Linux filesystem zup targets, and it counts bytes
/// rather than characters, so a name of 255 multi-byte characters is too long.
const NAME_MAX: usize = 255;

/// Why a target path could not be lowered onto this host.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LinuxPathLoweringError {
    #[error("target path `{path}` targets `{target}`, not Linux")]
    UnsupportedTarget { target: String, path: String },

    #[error("Linux path component `{component}` is invalid: {reason}")]
    InvalidComponent { component: String, reason: String },

    #[error(transparent)]
    InvalidPath(#[from] TargetPathError),
}

/// The `Unix` spelling of `path`, or why it has none.
///
/// The counterpart to the Windows backend's own accessor: a target path that
/// cannot be read as Unix is a path for another target, and this is where that is
/// said rather than where it is worked around.
pub fn linux_target_path(path: &TargetPath) -> Option<&str> {
    (path.target().operating_system() == TargetOperatingSystem::Linux).then_some(path.as_str())
}

/// Lower a Linux target path onto this host's filesystem.
///
/// The lexical half is already settled by [`TargetPath`]; what is checked here is
/// the half that is a property of the filesystem rather than of the spelling.
/// `NAME_MAX` is the rule that matters and it is not modelled by `typed-path`,
/// which is why this exists rather than a `to_str_lossy`.
pub fn to_host_path(path: &TargetPath) -> Result<PathBuf, LinuxPathLoweringError> {
    let Some(text) = linux_target_path(path) else {
        return Err(LinuxPathLoweringError::UnsupportedTarget {
            target: path.target().to_string(),
            path: path.as_str().to_owned(),
        });
    };
    for component in typed_path::Utf8TypedPath::new(text, typed_path::PathType::Unix).components() {
        // The root is structural and is already validated; only names are judged.
        if component.is_normal() {
            validate_component(component.as_str())?;
        }
    }
    Ok(PathBuf::from(text))
}

/// A Linux target path built from a host path.
///
/// The direction that has to be guarded. `Path::new("C:\\Users\\dev")` is a
/// perfectly good *Unix* path - one file, in a directory called `C:\Users\dev`,
/// relative to the working directory - so accepting it silently would lower a
/// Windows-shaped spelling into a Linux location without ever saying so. A path
/// carrying a backslash separator or a drive-letter prefix is therefore refused
/// here, where the mistake is made, rather than being written somewhere surprising
/// later.
pub fn target_path_from_host(
    path: &Path,
    target: &TargetTriple,
) -> Result<TargetPath, LinuxPathLoweringError> {
    let text = path
        .to_str()
        .ok_or_else(|| LinuxPathLoweringError::InvalidComponent {
            component: path.display().to_string(),
            reason: "a path is not text".to_owned(),
        })?;
    if let Some(offender) = windows_shaped(text) {
        return Err(LinuxPathLoweringError::InvalidComponent {
            component: offender.to_owned(),
            reason: "a Linux path is separated by `/` and has no drive prefix".to_owned(),
        });
    }
    TargetPath::new(target.clone(), text).map_err(LinuxPathLoweringError::InvalidPath)
}

/// The substring that makes `text` a Windows shape rather than a Unix one, if any.
fn windows_shaped(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    // A drive prefix is `X:` at the start, or after a UNC server's leading
    // backslashes, which is exactly the case a backslash test alone would miss.
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Some(&text[..2]);
    }
    text.find('\\').map(|index| &text[index..])
}

/// Linux's own filename rules, applied to one component.
///
/// Linux forbids exactly one character in a name - NUL, which `TargetPath` has
/// already refused - and bounds its length. Everything else POSIX permits is
/// permitted here: a name may contain a space, a colon, or any punctuation a
/// project wants, because refusing them would be a Windows rule and this is not
/// Windows.
fn validate_component(name: &str) -> Result<(), LinuxPathLoweringError> {
    let invalid = |reason: &str| LinuxPathLoweringError::InvalidComponent {
        component: name.to_owned(),
        reason: reason.to_owned(),
    };
    if name.len() > NAME_MAX {
        return Err(invalid("a Linux name is at most 255 bytes"));
    }
    // Linux forbids exactly one character in a name - NUL - and bounds its length.
    // Everything else POSIX permits is permitted here: a name may contain a space,
    // a colon, or any punctuation a project wants, because refusing them would be a
    // Windows rule and this is not Windows.
    if name.contains('\0') {
        return Err(invalid("forbidden character"));
    }
    // A control character is refused on top of what POSIX allows, because a path
    // that reaches a shell, a manifest, or a desktop entry through one is a path
    // whose spelling has to be escaped by hand somewhere else.
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

    /// The whole point of the closedness: a path for another target is refused
    /// rather than reinterpreted, because interpreting it would name a location
    /// the caller never asked about.
    #[test]
    fn refuses_non_linux_target_lowering() {
        let windows = TargetPath::new(
            TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            r"C:\Acme",
        )
        .expect("a canonical Windows path");
        assert!(matches!(
            to_host_path(&windows),
            Err(LinuxPathLoweringError::UnsupportedTarget { .. })
        ));
        assert_eq!(linux_target_path(&windows), None);
    }

    /// A canonical Unix path is already the host path, so lowering is a view. The
    /// interesting part is what it accepts: everything POSIX permits, including
    /// the characters Windows forbids.
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

    /// The reverse direction is where the mistake is actually made. Every
    /// Windows-shaped spelling is refused, because on Unix each of them is a valid
    /// *filename* and would therefore be accepted and written.
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
            matches!(error, LinuxPathLoweringError::InvalidComponent { .. }),
            "{text:?} was not refused as a component: {error}"
        );
    }

    /// And the shapes that are genuinely Unix keep working, so the refusal above
    /// is about Windows spelling rather than about punctuation in general.
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

    /// Linux compares paths byte for byte, so two spellings that differ only in
    /// case are two locations. This is the portable model's own rule and is worth
    /// pinning here because it is the exact opposite of the Windows backend's.
    #[test]
    fn case_is_significant_on_this_target() {
        assert_ne!(target_path("/opt/Acme"), target_path("/opt/acme"));
        assert!(!target_path("/opt/Acme").equivalent(&target_path("/opt/acme")));
    }

    /// A relative path and a `..` are refused by `TargetPath` itself, before this
    /// module sees them. Lowering must not become the place those are accepted -
    /// and a `.` is *not* one of them, because a redundant component names the same
    /// place rather than a different one, and the portable model canonicalizes it
    /// away instead of refusing the path.
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
