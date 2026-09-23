//! Windows-specific destination name validation.

use zup_core::Template;

use crate::error::BuildError;

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Validate literal segments of a destination template for Windows filename rules.
///
/// Segments that contain `${...}` are skipped: their final value is not known yet.
pub fn validate_windows_destination(template: &Template) -> Result<(), BuildError> {
    let rendered = template.to_string();
    let destination = rendered.clone();

    for segment in rendered.split('/') {
        if segment.is_empty() || segment.contains("${") {
            continue;
        }
        validate_windows_segment(segment, &destination)?;
    }

    Ok(())
}

fn validate_windows_segment(segment: &str, destination: &str) -> Result<(), BuildError> {
    if segment.ends_with(' ') || segment.ends_with('.') {
        return Err(BuildError::InvalidWindowsDestinationName {
            segment: segment.to_owned(),
            destination: destination.to_owned(),
        });
    }

    if segment
        .chars()
        .any(|c| FORBIDDEN_CHARS.contains(&c) || c.is_control())
    {
        return Err(BuildError::InvalidWindowsDestinationName {
            segment: segment.to_owned(),
            destination: destination.to_owned(),
        });
    }

    if is_reserved(segment) {
        return Err(BuildError::InvalidWindowsDestinationName {
            segment: segment.to_owned(),
            destination: destination.to_owned(),
        });
    }

    Ok(())
}

fn is_reserved(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use zup_core::{RelativePath, Template};

    use super::*;

    fn dest(suffix: &str) -> Template {
        Template::parse("${install}")
            .unwrap()
            .join_relative(&RelativePath::new(suffix).unwrap())
    }

    #[test]
    fn accepts_normal_names() {
        assert!(validate_windows_destination(&dest("tools/acme.exe")).is_ok());
    }

    #[test]
    fn rejects_reserved() {
        assert!(validate_windows_destination(&dest("CON")).is_err());
        assert!(validate_windows_destination(&dest("nul.txt")).is_err());
        assert!(validate_windows_destination(&dest("com1.dat")).is_err());
    }

    #[test]
    fn rejects_trailing_dot_or_space() {
        assert!(validate_windows_destination(&dest("file.")).is_err());
        assert!(validate_windows_destination(&dest("file ")).is_err());
    }

    #[test]
    fn rejects_forbidden_chars() {
        assert!(validate_windows_destination(&dest("a:b")).is_err());
        assert!(validate_windows_destination(&dest("a?b")).is_err());
    }

    #[test]
    fn skips_unknown_template_segments() {
        let template = Template::parse("${install}/CON/${app.name}/file.txt").unwrap();
        // `CON` is a literal segment and must still be rejected.
        assert!(validate_windows_destination(&template).is_err());

        let template = Template::parse("${install}/${app.name}/file.txt").unwrap();
        assert!(validate_windows_destination(&template).is_ok());
    }
}
