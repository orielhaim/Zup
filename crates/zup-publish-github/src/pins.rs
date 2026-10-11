use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const LOCK_SCHEMA: u32 = 2;

pub const LOCK_PATH: &str = "github-actions.lock.json";

const LOCK_JSON: &str = include_str!("../../../github-actions.lock.json");

const INFRASTRUCTURE: &[&str] = &[
    "actions/setup-node",
    "dtolnay/rust-toolchain",
    "Swatinem/rust-cache",
    "taiki-e/install-action",
    "oven-sh/setup-bun",
];

const GENERATED: &[&str] = &[
    "actions/checkout",
    "actions/upload-artifact",
    "actions/download-artifact",
    "actions/attest",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPin {
    pub repository: String,
    pub version: String,
    pub sha: String,
    pub checked_at: String,
}

impl ActionPin {
    pub fn uses(&self) -> String {
        format!("{}@{}", self.repository, self.version)
    }

    pub fn is_series(&self) -> bool {
        self.version
            .strip_prefix('v')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    }

    pub fn series(&self) -> Option<u64> {
        self.version.strip_prefix('v')?.parse().ok()
    }
}

impl fmt::Display for ActionPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.uses())
    }
}

pub fn pins() -> &'static [ActionPin] {
    static PINS: std::sync::OnceLock<Vec<ActionPin>> = std::sync::OnceLock::new();
    PINS.get_or_init(|| {
        let lock: PinLock = serde_json::from_str(LOCK_JSON).expect("github-actions.lock.json");
        lock.pins()
    })
}

pub fn lock() -> PinLock {
    serde_json::from_str(LOCK_JSON).expect("github-actions.lock.json")
}

pub fn lock_json() -> &'static str {
    LOCK_JSON
}

pub fn pin(repository: &str) -> Result<ActionPin, PinError> {
    pins()
        .iter()
        .find(|pin| pin.repository == repository)
        .cloned()
        .ok_or_else(|| PinError::Unknown {
            repository: repository.to_owned(),
        })
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PinError {
    #[error(
        "`{repository}` is not in {LOCK_PATH}; add it with `cargo xtask github-action-pins refresh --add {repository}`"
    )]
    Unknown { repository: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinLock {
    pub schema: u32,
    #[serde(rename = "$comment", default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<serde_json::Value>,
    pub actions: BTreeMap<String, LockedAction>,
}

impl PinLock {
    pub fn pins(&self) -> Vec<ActionPin> {
        self.actions
            .iter()
            .map(|(repository, action)| ActionPin {
                repository: repository.clone(),
                version: action.version.clone(),
                sha: action.sha.clone(),
                checked_at: action.checked_at.clone(),
            })
            .collect()
    }

    pub fn encode(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a lock serializes");
        text.push('\n');
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedAction {
    pub version: String,
    pub sha: String,
    #[serde(rename = "checkedAt")]
    pub checked_at: String,
}

pub fn generated_actions() -> &'static [&'static str] {
    GENERATED
}

pub fn infrastructure_actions() -> &'static [&'static str] {
    INFRASTRUCTURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pinned_action_is_resolvable() {
        for action in GENERATED {
            assert!(pin(action).is_ok(), "{action} is missing from {LOCK_PATH}");
        }
        for action in INFRASTRUCTURE {
            assert!(pin(action).is_ok(), "{action} is missing from {LOCK_PATH}");
        }
    }

    #[test]
    fn pins_are_ordered_and_unique() {
        let pins = pins();
        let mut repositories: Vec<&str> = pins.iter().map(|pin| pin.repository.as_str()).collect();
        let sorted = {
            let mut sorted = repositories.clone();
            sorted.sort_unstable();
            sorted
        };
        assert_eq!(repositories, sorted, "the lock is not in repository order");
        repositories.dedup();
        assert_eq!(repositories.len(), pins.len(), "a pin is listed twice");
    }

    #[test]
    fn only_a_bare_major_series_is_a_series() {
        let numbered = |version: &str| ActionPin {
            repository: "example/action".to_owned(),
            version: version.to_owned(),
            sha: "0".repeat(40),
            checked_at: "2026-09-27".to_owned(),
        };
        for (version, expected) in [("v1", Some(1u64)), ("v7", Some(7)), ("v12", Some(12))] {
            let pin = numbered(version);
            assert!(pin.is_series(), "{version} is a major series");
            assert_eq!(pin.series(), expected);
        }
        for version in ["stable", "nightly", "v7.0.1", "main", "v"] {
            let pin = numbered(version);
            assert!(!pin.is_series(), "{version} is not a moving major series");
            assert_eq!(pin.series(), None, "{version} has no series number");
        }
    }
    #[test]
    fn every_tracked_ref_is_one_a_workflow_can_spell() {
        for action in pins() {
            let uses = action.uses();
            let ref_part = uses.split_once('@').expect("a ref").1;
            assert!(
                !ref_part.contains(char::is_whitespace)
                    && !ref_part.contains('#')
                    && !ref_part.is_empty(),
                "{uses} is not a usable ref"
            );
        }
    }

    #[test]
    fn an_unknown_action_is_an_error_rather_than_a_panic() {
        let error = pin("someone/does-not-exist").expect_err("the lock has no such entry");
        assert!(matches!(error, PinError::Unknown { .. }));
        assert!(error.to_string().contains("github-action-pins refresh"));
    }

    #[test]
    fn the_lock_rejects_a_field_it_does_not_know() {
        let error = serde_json::from_str::<PinLock>(r#"{"schema":2,"actions":{},"surprise":true}"#)
            .expect_err("an unknown field is a lock someone edited by hand");
        assert!(error.to_string().contains("surprise"));
    }
}
