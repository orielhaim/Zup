//! Windows persistent search-path (`PATH`) semantics.
//!
//! This module owns everything about how Windows stores and compares a search
//! path: the `;` separator, surrounding whitespace, quoting, case-insensitive
//! path identity, `%VAR%` segments that must never be treated as a concrete
//! directory, and the `REG_SZ` / `REG_EXPAND_SZ` value types. Everything
//! upstream of this module sees only [`SearchPath`] values.

use windows_registry::{Key, Type};
use zup_core::{SelectedScope, TargetTriple};
use zup_exec::SearchPath;
use zup_platform::TargetPath;

use crate::registry::{RegistryError, RegistryReader, RegistryValue};

/// Value name Windows stores a search path under.
pub const PATH_VALUE_NAME: &str = "Path";

/// `REG_SZ`: a search path with no variable references.
pub const VALUE_TYPE_PLAIN: &str = "sz";

/// `REG_EXPAND_SZ`: a search path the host expands at command launch.
pub const VALUE_TYPE_EXPAND: &str = "expand_sz";

/// Reported when a search path value does not exist yet.
pub const VALUE_TYPE_MISSING: &str = "missing";

/// Split a stored search-path value into raw segments.
///
/// Windows separates segments with `;` and ignores surrounding whitespace.
/// Segments that are still empty after trimming name no directory and are
/// dropped.
pub fn split(value: &str) -> Vec<&str> {
    value
        .split(';')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// True when a stored segment is an unexpanded variable reference rather than a
/// concrete directory.
///
/// `%ProgramFiles%\Acme` names a directory a resolved path would also name, but
/// expanding it is the host's job at command launch. zup never treats one as
/// equal to a concrete path, so an entry it owns cannot be shadowed by a
/// variable reference.
pub fn is_variable_reference(segment: &str) -> bool {
    segment.contains('%')
}

/// Strip the quoting Windows accepts around a segment containing spaces.
pub fn unquote(segment: &str) -> &str {
    let trimmed = segment.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim()
}

/// Parse one stored segment into a target path.
///
/// Returns `None` for a variable reference, a relative segment, or anything the
/// target rejects, so those segments never appear in a [`SearchPath`].
pub fn to_target_path(target: &TargetTriple, segment: &str) -> Option<TargetPath> {
    let segment = unquote(segment);
    if segment.is_empty() || is_variable_reference(segment) {
        return None;
    }
    TargetPath::new(target, segment).ok()
}

/// Build the portable, target-normalized view of a stored search-path value.
///
/// Segments that cannot name a target path are dropped, so membership
/// questions only ever consider concrete directories.
pub fn collect(target: &TargetTriple, value: &str) -> SearchPath {
    split(value)
        .into_iter()
        .filter_map(|segment| to_target_path(target, segment))
        .collect()
}

/// True when `desired` is already a member of the stored search path.
///
/// Identity is Windows path identity: separators and case do not matter, but a
/// `%VAR%` segment never matches a concrete path.
pub fn contains(target: &TargetTriple, value: &str, desired: &TargetPath) -> bool {
    split(value)
        .into_iter()
        .filter_map(|segment| to_target_path(target, segment))
        .any(|stored| stored.equivalent(desired))
}

/// Read the persistent search path for `scope`, with its stored type.
///
/// `REG_SZ` and `REG_EXPAND_SZ` are both readable as a string; any other type is
/// reported as missing rather than guessed at.
pub fn read<R: RegistryReader>(
    reader: &R,
    scope: SelectedScope,
) -> Result<Option<(String, String)>, RegistryError> {
    let Some(key) = reader.open_environment_key(scope)? else {
        return Ok(None);
    };
    Ok(read_key(&key))
}

/// Read a search path straight from an opened environment key.
pub fn read_key(key: &Key) -> Option<(String, String)> {
    let value = key.get_value(PATH_VALUE_NAME).ok()?;
    let kind = match value.ty() {
        Type::String => RegistryValue::Sz(String::new()),
        Type::ExpandString => RegistryValue::ExpandSz(String::new()),
        _ => {
            return None;
        }
    };
    let text = String::try_from(value).ok()?;
    Some((value_type(kind).to_owned(), text))
}

/// Name the registry type that describes a stored search path.
pub fn value_type(value: RegistryValue) -> &'static str {
    match value {
        RegistryValue::Sz(_) => VALUE_TYPE_PLAIN,
        RegistryValue::ExpandSz(_) => VALUE_TYPE_EXPAND,
        RegistryValue::Missing | RegistryValue::Other { .. } => VALUE_TYPE_MISSING,
    }
}

/// Type a search path should be written back with.
///
/// Windows stores a search path containing `%VAR%` segments as `REG_EXPAND_SZ`.
/// Every other case follows whatever the host already used, so zup never
/// silently changes how the host expands the value.
pub fn write_value_type(previous: &str) -> &'static str {
    if previous == VALUE_TYPE_PLAIN {
        VALUE_TYPE_PLAIN
    } else {
        VALUE_TYPE_EXPAND
    }
}

/// True when a type change means the host started expanding a plain value.
pub fn lost_expansion(previous: &str, current: &str) -> bool {
    previous == VALUE_TYPE_MISSING && current != VALUE_TYPE_PLAIN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows() -> TargetTriple {
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    }

    #[test]
    fn split_drops_empty_segments_and_trims() {
        assert_eq!(split(r"  C:\one ; ;C:\two;; "), vec![r"C:\one", r"C:\two"]);
        assert!(split("").is_empty());
        assert!(split(";;;;").is_empty());
    }

    #[test]
    fn variable_references_never_become_target_paths() {
        let target = windows();
        assert!(is_variable_reference(r"%ProgramFiles%\Acme"));
        assert!(to_target_path(&target, r"%ProgramFiles%\Acme").is_none());
        assert_eq!(
            collect(&target, r"%ProgramFiles%\Acme;C:\Apps\Acme").len(),
            1
        );
    }

    #[test]
    fn quoting_and_case_are_windows_identity() {
        let target = windows();
        let desired = TargetPath::new(&target, r"C:\Apps\bin").unwrap();
        assert!(contains(&target, r"c:/apps/bin", &desired));
        assert!(contains(&target, r"C:\Apps\bin\", &desired));
        assert!(contains(&target, r#""C:\Apps\bin""#, &desired));
        assert!(!contains(&target, r"%SOMETHING%\bin", &desired));
    }

    #[test]
    fn write_type_never_downgrades_an_expanding_path() {
        assert_eq!(write_value_type(VALUE_TYPE_EXPAND), VALUE_TYPE_EXPAND);
        assert_eq!(write_value_type(VALUE_TYPE_PLAIN), VALUE_TYPE_PLAIN);
        assert_eq!(write_value_type(VALUE_TYPE_MISSING), VALUE_TYPE_EXPAND);
        // Losing a value that used to expand is drift the caller has to see, not
        // a silent promotion of a plain value.
        assert!(lost_expansion(VALUE_TYPE_MISSING, VALUE_TYPE_EXPAND));
        assert!(!lost_expansion(VALUE_TYPE_PLAIN, VALUE_TYPE_PLAIN));
        assert!(!lost_expansion(VALUE_TYPE_EXPAND, VALUE_TYPE_EXPAND));
    }
}
