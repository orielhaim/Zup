//! Validate shortcut display names as Windows filename components.

/// Reject separators, reserved device names, trailing dots/spaces, and
/// forbidden filename characters. Does not sanitize.
pub fn validate_shortcut_filename(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty name".to_owned());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("path separators are not allowed".to_owned());
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err("trailing space or dot is not allowed".to_owned());
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return Err("forbidden filename character".to_owned());
    }

    let stem = name.split('.').next().unwrap_or(name);
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return Err("reserved device name".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A shortcut name becomes a file name, so the reserved device names and the
    // separator and trailing-dot rules that make a name unrepresentable on
    // Windows all have to be refused before anything is written.
    #[test]
    fn shortcut_filenames_are_validated() {
        for (name, accepted) in [
            ("Acme", true),
            ("Acme App", true),
            ("a/b", false),
            ("a\\b", false),
            ("CON", false),
            ("file.", false),
            ("file ", false),
            ("a:b", false),
        ] {
            assert_eq!(
                validate_shortcut_filename(name).is_ok(),
                accepted,
                "{name:?}"
            );
        }
    }
}
