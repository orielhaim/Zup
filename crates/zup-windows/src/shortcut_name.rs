pub const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShortcutNameError {
    #[error("empty name")]
    Empty,
    #[error("path separators are not allowed")]
    Separator,
    #[error("trailing space or dot is not allowed")]
    Trailing,
    #[error("forbidden filename character")]
    Forbidden,
    #[error("reserved device name")]
    Reserved,
}

pub fn is_reserved_device_stem(stem: &str) -> bool {
    RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

pub fn validate_shortcut_filename(name: &str) -> Result<(), ShortcutNameError> {
    if name.is_empty() {
        return Err(ShortcutNameError::Empty);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(ShortcutNameError::Separator);
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(ShortcutNameError::Trailing);
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return Err(ShortcutNameError::Forbidden);
    }
    let stem = name.split('.').next().unwrap_or(name);
    if is_reserved_device_stem(stem) {
        return Err(ShortcutNameError::Reserved);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("Acme", true)]
    #[case("Acme App", true)]
    #[case("a/b", false)]
    #[case("a\\b", false)]
    #[case("CON", false)]
    #[case("nul.txt", false)]
    #[case("file.", false)]
    #[case("file ", false)]
    #[case("a:b", false)]
    fn shortcut_filenames_are_validated(#[case] name: &str, #[case] accepted: bool) {
        assert_eq!(validate_shortcut_filename(name).is_ok(), accepted);
    }
}
