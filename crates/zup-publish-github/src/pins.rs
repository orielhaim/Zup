//! The GitHub Actions zup's own workflows depend on, and how that set stays current.
//!
//! Workflows read `actions/checkout@v7`, not a full commit SHA: that is the
//! convention the ecosystem uses, the thing dependabot advances, and the thing a
//! reviewer recognises without a lookup.
//!
//! The cost is that a tag can move, and a moved tag is invisible in a diff. So
//! `github-actions.lock.json` records, per ref, the commit it resolved to and the
//! date it was checked, and `cargo xtask github-action-pins check --online` reports
//! a ref that now points somewhere else. That is the lock's real job; one that only
//! stored refs would be a list rather than a check. It also enumerates the action
//! set, so a workflow pulling in an unlisted third-party action is reported, and
//! `refresh` advances a series in one place instead of a developer typing a new tag
//! into four files.
//!
//! The lock is compiled in, so `zup ci github generate` reads it without touching
//! the filesystem and stays byte-reproducible: regenerating a matrix never moves a
//! ref. A third-party marketplace action in a release pipeline is a dependency the
//! project did not choose, so every entry here has to justify itself in review.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The lock format this build understands.
///
/// Bumped to 2 when `version` stopped being a commit SHA and became a ref, which
/// is a different field with a different meaning. `check` refuses a lock it does
/// not understand rather than reading a `version` that meant something else.
pub const LOCK_SCHEMA: u32 = 2;

/// The lock file's path, relative to the repository root.
pub const LOCK_PATH: &str = "github-actions.lock.json";

/// The lock, compiled in so generation needs no filesystem access.
const LOCK_JSON: &str = include_str!("../../../github-actions.lock.json");

/// The actions zup's workflows use but a project's generated workflow does not.
///
/// Reported as "known to zup but not referenced" rather than as errors: the zup
/// repository's own CI needs toolchain actions a consumer's release pipeline has no
/// reason to pull in.
const INFRASTRUCTURE: &[&str] = &[
    "actions/setup-node",
    "dtolnay/rust-toolchain",
    "Swatinem/rust-cache",
    "taiki-e/install-action",
    // The action's toolchain: Bun builds and tests the action, Node 24 runs it.
    "oven-sh/setup-bun",
];

/// The actions a generated release workflow may use.
///
/// A project that turns off attestation does not use `actions/attest`, so this is
/// the set a workflow may reference rather than one it must.
const GENERATED: &[&str] = &[
    "actions/checkout",
    "actions/upload-artifact",
    "actions/download-artifact",
    "actions/attest",
];

/// One tracked action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPin {
    /// `owner/name`.
    pub repository: String,
    /// The ref a workflow uses after `@`: a major series like `v7`, or a moving
    /// channel like `stable`.
    ///
    /// Stored verbatim rather than normalised, because the ref is what appears in a
    /// workflow and a lock that renders differently from what it tracks is a lock
    /// somebody has to translate in their head.
    pub version: String,
    /// The commit this ref resolved to on `checked_at`.
    ///
    /// A record, not a constraint: its job is to make a moved tag visible, which is
    /// the one failure mode a version ref cannot report on its own.
    pub sha: String,
    /// When the ref was last resolved against upstream.
    pub checked_at: String,
}

impl ActionPin {
    /// The `uses:` line, the way the ecosystem writes one: `actions/checkout@v7`.
    pub fn uses(&self) -> String {
        format!("{}@{}", self.repository, self.version)
    }

    /// Whether this ref is a major series that moves, like `v7`.
    pub fn is_series(&self) -> bool {
        self.version
            .strip_prefix('v')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    }

    /// The series number, for a ref that is one.
    pub fn series(&self) -> Option<u64> {
        self.version.strip_prefix('v')?.parse().ok()
    }
}

impl fmt::Display for ActionPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.uses())
    }
}

/// Every tracked action, in repository order.
pub fn pins() -> &'static [ActionPin] {
    static PINS: std::sync::OnceLock<Vec<ActionPin>> = std::sync::OnceLock::new();
    PINS.get_or_init(|| {
        let lock: PinLock = serde_json::from_str(LOCK_JSON).expect("github-actions.lock.json");
        lock.pins()
    })
}

/// The lock as it is written, for `check` and `refresh` to report on.
pub fn lock() -> PinLock {
    serde_json::from_str(LOCK_JSON).expect("github-actions.lock.json")
}

/// The raw lock text, so `refresh` can preserve the `$comment` and key order a
/// human chose rather than reserializing the file into something else.
pub fn lock_json() -> &'static str {
    LOCK_JSON
}

/// The pin for one action, or why there is none.
///
/// A `Result` rather than a panic: the previous table's `pin()` panicked, so a
/// generator naming a new action crashed with a message about a `const` table
/// rather than about a lock file somebody has to edit.
pub fn pin(repository: &str) -> Result<ActionPin, PinError> {
    pins()
        .iter()
        .find(|pin| pin.repository == repository)
        .cloned()
        .ok_or_else(|| PinError::Unknown {
            repository: repository.to_owned(),
        })
}

/// Why a lock could not answer a question.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PinError {
    /// The lock names no action by this name.
    #[error(
        "`{repository}` is not in {LOCK_PATH}; add it with `cargo xtask github-action-pins refresh --add {repository}`"
    )]
    Unknown {
        /// The action that was asked for.
        repository: String,
    },
}

/// The lock file's shape.
///
/// `$comment` is allowed and ignored: the file needs to explain itself to whoever
/// opens it next, and `$comment` is the one conventional non-hack place for that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinLock {
    /// The format version.
    pub schema: u32,
    /// Why this file exists, for whoever edits it next.
    #[serde(rename = "$comment", default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<serde_json::Value>,
    /// Every tracked action, keyed by `owner/name`.
    pub actions: BTreeMap<String, LockedAction>,
}

impl PinLock {
    /// The pins, in repository order.
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

    /// The lock as bytes, with the shape a diff can be read against.
    pub fn encode(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a lock serializes");
        text.push('\n');
        text
    }
}

/// One locked action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedAction {
    /// The ref a workflow uses after `@`.
    pub version: String,
    /// The commit this ref resolved to when it was last checked.
    pub sha: String,
    /// When the ref was last resolved against upstream, as `YYYY-MM-DD`.
    ///
    /// `checkedAt` rather than `checked_at` because the file is read by humans and
    /// by `jq` as often as by this crate, and camelCase is what the rest of the JSON
    /// zup writes uses.
    #[serde(rename = "checkedAt")]
    pub checked_at: String,
}

/// The actions a generated workflow may use.
pub fn generated_actions() -> &'static [&'static str] {
    GENERATED
}

/// The actions only zup's own CI uses.
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
    fn a_ref_renders_as_the_ecosystem_writes_it() {
        let pin = pin("actions/checkout").expect("checkout is tracked");
        assert_eq!(pin.uses(), "actions/checkout@v7");
        assert_eq!(pin.version, "v7");
        assert_eq!(pin.uses(), format!("{}@{}", pin.repository, pin.version));
        // No trailing comment: the version is in the ref, so a second copy of it
        // beside the ref is a second thing that can disagree.
        assert!(!pin.uses().contains('#'));
    }

    #[test]
    fn a_series_is_recognised_and_numbered() {
        for (version, expected) in [("v1", Some(1)), ("v7", Some(7)), ("v12", Some(12))] {
            let pin = ActionPin {
                repository: "example/action".to_owned(),
                version: version.to_owned(),
                sha: "0".repeat(40),
                checked_at: "2026-09-27".to_owned(),
            };
            assert!(pin.is_series(), "{version} is a major series");
            assert_eq!(pin.series(), expected);
        }
    }

    #[test]
    fn a_channel_is_not_a_series() {
        // `stable` has no version to compare against and `v7.0.1` is a fixed
        // release rather than something that moves. Only a bare `v<n>` is a series.
        for version in ["stable", "nightly", "v7.0.1", "main", "v"] {
            let pin = ActionPin {
                repository: "example/action".to_owned(),
                version: version.to_owned(),
                sha: "0".repeat(40),
                checked_at: "2026-09-27".to_owned(),
            };
            assert!(!pin.is_series(), "{version} is not a moving major series");
            assert_eq!(pin.series(), None, "{version} has no series number");
        }
    }

    #[test]
    fn a_channel_ref_renders_verbatim() {
        let pin = pin("dtolnay/rust-toolchain").expect("rust-toolchain is tracked");
        assert_eq!(pin.uses(), "dtolnay/rust-toolchain@stable");
        assert!(!pin.is_series());
    }

    #[test]
    fn every_tracked_ref_is_one_a_workflow_can_spell() {
        // A ref with a space or a comment in it renders a `uses:` line GitHub cannot
        // parse, and the failure surfaces on a runner rather than here.
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
    fn the_lock_round_trips() {
        let lock = lock();
        let encoded = lock.encode();
        let decoded: PinLock = serde_json::from_str(&encoded).expect("the lock round trips");
        assert_eq!(decoded, lock);
    }

    #[test]
    fn the_lock_rejects_a_field_it_does_not_know() {
        let error = serde_json::from_str::<PinLock>(r#"{"schema":2,"actions":{},"surprise":true}"#)
            .expect_err("an unknown field is a lock someone edited by hand");
        assert!(error.to_string().contains("surprise"));
    }
}
