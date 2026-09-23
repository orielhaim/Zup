//! Read-only registry inspection helpers (`windows-registry`).
//!
//! **No writes** of any kind during inspection.

use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};
use zup_core::SelectedScope;

/// Registry value with explicit type information preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryValue {
    Missing,
    Sz(String),
    ExpandSz(String),
    Other { kind: u32, raw_len: usize },
}

/// Read-only registry access surface for inspection and tests.
pub trait RegistryReader {
    fn open_classes_key(
        &self,
        scope: SelectedScope,
        subkey: &str,
    ) -> Result<Option<Key>, RegistryError>;

    fn open_environment_key(&self, scope: SelectedScope) -> Result<Option<Key>, RegistryError>;

    fn read_value(key: &Key, name: &str) -> RegistryValue;
}

/// Registry inspection errors.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry read failed: {0}")]
    Failed(String),
}

/// Production registry reader (native/current view).
///
/// `Software\Classes` and `Environment` use the process-native view. Extensible
/// when target architecture enters the model.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsRegistryReader;

impl RegistryReader for WindowsRegistryReader {
    fn open_classes_key(
        &self,
        scope: SelectedScope,
        subkey: &str,
    ) -> Result<Option<Key>, RegistryError> {
        let root = match scope {
            SelectedScope::User => CURRENT_USER
                .open("Software\\Classes")
                .map_err(|e| RegistryError::Failed(e.message().to_owned()))?,
            SelectedScope::Machine => LOCAL_MACHINE
                .open("Software\\Classes")
                .map_err(|e| RegistryError::Failed(e.message().to_owned()))?,
        };
        match root.open(subkey) {
            Ok(key) => Ok(Some(key)),
            Err(_) => Ok(None),
        }
    }

    fn open_environment_key(&self, scope: SelectedScope) -> Result<Option<Key>, RegistryError> {
        let result = match scope {
            SelectedScope::User => CURRENT_USER.open("Environment"),
            SelectedScope::Machine => LOCAL_MACHINE
                .open("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"),
        };
        match result {
            Ok(key) => Ok(Some(key)),
            Err(_) => Ok(None),
        }
    }

    fn read_value(key: &Key, name: &str) -> RegistryValue {
        let Ok(value) = key.get_value(name) else {
            return RegistryValue::Missing;
        };
        match value.ty() {
            Type::String => match String::try_from(value) {
                Ok(s) => RegistryValue::Sz(s),
                Err(_) => RegistryValue::Other {
                    kind: 1,
                    raw_len: 0,
                },
            },
            Type::ExpandString => match String::try_from(value) {
                Ok(s) => RegistryValue::ExpandSz(s),
                Err(_) => RegistryValue::Other {
                    kind: 2,
                    raw_len: 0,
                },
            },
            _ => RegistryValue::Other {
                kind: 0,
                raw_len: value.len(),
            },
        }
    }
}

/// Split a PATH value into raw entries using Windows `;` semantics.
pub fn split_path_value(value: &str) -> Vec<&str> {
    value
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Read the persistent PATH string for `scope`, if present.
pub fn read_path_value<R: RegistryReader>(
    reader: &R,
    scope: SelectedScope,
) -> Result<Option<String>, RegistryError> {
    let Some(key) = reader.open_environment_key(scope)? else {
        return Ok(None);
    };
    match R::read_value(&key, "Path") {
        RegistryValue::Sz(s) | RegistryValue::ExpandSz(s) => Ok(Some(s)),
        RegistryValue::Missing | RegistryValue::Other { .. } => Ok(None),
    }
}
