use serde::{Deserialize, Serialize};
use zup_core::{ComponentId, Condition, PluginId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub id: PluginId,
    pub source: String,
    #[serde(default)]
    pub component: Option<ComponentId>,
    #[serde(default)]
    pub when: Option<Condition>,
}

pub(crate) fn is_valid_source(source: &str) -> bool {
    if source.is_empty()
        || source.contains('\0')
        || source.contains('\\')
        || source.starts_with('/')
    {
        return false;
    }

    let bytes = source.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }

    source
        .split('/')
        .all(|component| !component.is_empty() && component != "." && component != "..")
}
