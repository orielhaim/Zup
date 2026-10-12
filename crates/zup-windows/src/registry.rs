use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};
use zup_core::SelectedScope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryValue {
    Missing,
    Sz(String),
    ExpandSz(String),
    Other { kind: u32, raw_len: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry read failed: {0}")]
    Failed(String),
}

pub fn open_classes_key(scope: SelectedScope, subkey: &str) -> Result<Option<Key>, RegistryError> {
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

pub fn open_environment_key(scope: SelectedScope) -> Result<Option<Key>, RegistryError> {
    let result = match scope {
        SelectedScope::User => CURRENT_USER.open("Environment"),
        SelectedScope::Machine => {
            LOCAL_MACHINE.open("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")
        }
    };
    match result {
        Ok(key) => Ok(Some(key)),
        Err(_) => Ok(None),
    }
}

pub fn read_value(key: &Key, name: &str) -> RegistryValue {
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
