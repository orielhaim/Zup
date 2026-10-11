use windows_registry::{Key, Type};
use zup_core::{SelectedScope, TargetTriple};
use zup_exec::SearchPath;
use zup_platform::TargetPath;

use crate::registry::{RegistryError, RegistryValue};

pub const PATH_VALUE_NAME: &str = "Path";

pub const VALUE_TYPE_PLAIN: &str = "sz";

pub const VALUE_TYPE_EXPAND: &str = "expand_sz";

pub const VALUE_TYPE_MISSING: &str = "missing";

pub fn split(value: &str) -> Vec<&str> {
    value
        .split(';')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

pub fn is_variable_reference(segment: &str) -> bool {
    segment.contains('%')
}

pub fn unquote(segment: &str) -> &str {
    let trimmed = segment.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim()
}

pub fn to_target_path(target: &TargetTriple, segment: &str) -> Option<TargetPath> {
    let segment = unquote(segment);
    if segment.is_empty() || is_variable_reference(segment) {
        return None;
    }
    TargetPath::new(target, segment).ok()
}

pub fn collect(target: &TargetTriple, value: &str) -> SearchPath {
    split(value)
        .into_iter()
        .filter_map(|segment| to_target_path(target, segment))
        .collect()
}

pub fn contains(target: &TargetTriple, value: &str, desired: &TargetPath) -> bool {
    split(value)
        .into_iter()
        .filter_map(|segment| to_target_path(target, segment))
        .any(|stored| stored.equivalent(desired))
}

pub fn read(scope: SelectedScope) -> Result<Option<(String, String)>, RegistryError> {
    let Some(key) = crate::registry::open_environment_key(scope)? else {
        return Ok(None);
    };
    Ok(read_key(&key))
}

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

pub fn value_type(value: RegistryValue) -> &'static str {
    match value {
        RegistryValue::Sz(_) => VALUE_TYPE_PLAIN,
        RegistryValue::ExpandSz(_) => VALUE_TYPE_EXPAND,
        RegistryValue::Missing | RegistryValue::Other { .. } => VALUE_TYPE_MISSING,
    }
}

pub fn write_value_type(previous: &str) -> &'static str {
    if previous == VALUE_TYPE_PLAIN {
        VALUE_TYPE_PLAIN
    } else {
        VALUE_TYPE_EXPAND
    }
}

pub fn lost_expansion(previous: &str, current: &str) -> bool {
    previous == VALUE_TYPE_MISSING && current != VALUE_TYPE_PLAIN
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn windows() -> TargetTriple {
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    }

    #[rstest]
    #[case(r"  C:\one ; ;C:\two;; ", vec![r"C:\one", r"C:\two"])]
    #[case("", vec![])]
    #[case(";;;;", vec![])]
    fn split_drops_empty_segments_and_trims(#[case] value: &str, #[case] expected: Vec<&str>) {
        assert_eq!(split(value), expected);
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

    #[rstest]
    #[case(r"c:/apps/bin", true)]
    #[case(r"C:\Apps\bin\", true)]
    #[case(r#""C:\Apps\bin""#, true)]
    #[case(r"%SOMETHING%\bin", false)]
    fn quoting_and_case_are_windows_identity(#[case] stored: &str, #[case] found: bool) {
        let target = windows();
        let desired = TargetPath::new(&target, r"C:\Apps\bin").unwrap();
        assert_eq!(contains(&target, stored, &desired), found);
    }

    #[rstest]
    #[case(VALUE_TYPE_EXPAND, VALUE_TYPE_EXPAND)]
    #[case(VALUE_TYPE_PLAIN, VALUE_TYPE_PLAIN)]
    #[case(VALUE_TYPE_MISSING, VALUE_TYPE_EXPAND)]
    fn write_type_never_downgrades_an_expanding_path(#[case] previous: &str, #[case] kept: &str) {
        assert_eq!(write_value_type(previous), kept);
    }

    #[rstest]
    #[case(VALUE_TYPE_MISSING, VALUE_TYPE_EXPAND, true)]
    #[case(VALUE_TYPE_PLAIN, VALUE_TYPE_PLAIN, false)]
    #[case(VALUE_TYPE_EXPAND, VALUE_TYPE_EXPAND, false)]
    fn expansion_loss_is_detected(
        #[case] previous: &str,
        #[case] current: &str,
        #[case] lost: bool,
    ) {
        assert_eq!(lost_expansion(previous, current), lost);
    }
}
