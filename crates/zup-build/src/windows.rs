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
/// Segments that contain `${...}` are not interpreted as literal names; their literal syntax is still checked.
pub fn validate_windows_destination(template: &Template) -> Result<(), BuildError> {
    let rendered = template.to_string();
    if rendered.starts_with('/') || rendered.starts_with('\\') {
        return Err(invalid_destination_segment("<root>", &rendered));
    }

    for segment in rendered.split('/') {
        if segment.is_empty() {
            return Err(invalid_destination_segment("<empty>", &rendered));
        }
        if segment.contains("${") {
            validate_variable_segment(segment, &rendered)?;
        } else {
            validate_windows_segment(segment, &rendered)?;
        }
    }

    Ok(())
}

fn validate_variable_segment(segment: &str, destination: &str) -> Result<(), BuildError> {
    let mut remaining = segment;
    while let Some(start) = remaining.find("${") {
        let literal = &remaining[..start];
        if !literal.is_empty() {
            validate_windows_segment(literal, destination)?;
        }
        let after_variable = &remaining[start + 2..];
        let Some(end) = after_variable.find('}') else {
            return Err(invalid_destination_segment(segment, destination));
        };
        remaining = &after_variable[end + 1..];
    }
    if !remaining.is_empty() {
        validate_windows_segment(remaining, destination)?;
    }
    Ok(())
}

fn invalid_destination_segment(segment: &str, destination: &str) -> BuildError {
    BuildError::InvalidWindowsDestinationName {
        segment: segment.to_owned(),
        destination: destination.to_owned(),
    }
}

fn validate_windows_segment(segment: &str, destination: &str) -> Result<(), BuildError> {
    if segment.ends_with(' ') || segment.ends_with('.') {
        return Err(invalid_destination_segment(segment, destination));
    }

    if segment
        .chars()
        .any(|c| FORBIDDEN_CHARS.contains(&c) || c.is_control())
    {
        return Err(invalid_destination_segment(segment, destination));
    }

    if is_reserved(segment) {
        return Err(invalid_destination_segment(segment, destination));
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
        assert!(validate_windows_destination(&template).is_err());

        let template = Template::parse("${install}/${app.name}/file.txt").unwrap();
        assert!(validate_windows_destination(&template).is_ok());
    }

    #[test]
    fn validates_literal_fragments_around_variables() {
        for value in [
            r"C:\evil${install}",
            "prefix${app.name}:suffix",
            "${app.name}CON",
            "CON${app.name}",
            "${app.name}COM1.txt",
            "prefix${app.name}.",
            "prefix${app.name}\u{0001}",
        ] {
            let template = Template::parse(value).unwrap();
            assert!(
                validate_windows_destination(&template).is_err(),
                "{value:?}"
            );
        }

        let template = Template::parse("prefix${app.name}middle${app.version}.exe").unwrap();
        assert!(validate_windows_destination(&template).is_ok());
    }

    #[test]
    fn rejects_device_unc_and_control_paths() {
        for value in [
            r"\\.\PIPE\device",
            r"\\server\share\file.txt",
            "C:/file.txt",
            "file\u{0001}.txt",
        ] {
            let template = Template::parse(value).unwrap();
            assert!(
                validate_windows_destination(&template).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn allows_template_segments_without_weakening_literal_checks() {
        let template = Template::parse("${install}/${app.name}/file.txt").unwrap();
        assert!(validate_windows_destination(&template).is_ok());

        let template = Template::parse("${install}/CON/file.txt").unwrap();
        assert!(validate_windows_destination(&template).is_err());
    }
}
