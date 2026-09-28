//! Diagnostics: what zup wants an integration to know, and where.
//!
//! One shape for every message a command reports, whether it came from a typed
//! failure, from a `miette` diagnostic, or from a check that failed inside a
//! read-only report. The three levels are the three a CI runner can render, and a
//! fourth would have nowhere to go: a consumer that meets an unknown severity shows it
//! as a notice, because a diagnostic zup invented a level for must not fail a release
//! that otherwise succeeded.
//!
//! The `code` is the part an integration can act on. It is a stable identifier in a
//! closed *shape* and an open *vocabulary*:
//!
//! ```text
//! zup.manifest.unknown_target
//! zup.toolchain.component_mismatch
//! zup.publish.asset_conflict
//! zup.signing.untrusted_chain
//! zup.build.plugin_compile_failed
//! ```
//!
//! The subject is the area, the words are kebab-case, and the whole thing is one
//! `Identifier`. A Rust type name is not a code: `zup_manifest::UnknownTarget` renames
//! the day the module is reorganized, and an integration that matched on it would break
//! on a refactor that changed nothing an operator could see.
//!
//! [`zup.codes`](https://docs.rs) aside, the fallback is [`FALLBACK_CODE`]. An older or
//! internal error with no typed metadata still reaches a consumer as a real diagnostic
//! with a real message - it just cannot be matched on, which is the honest outcome.

use serde::{Deserialize, Serialize};

use crate::identifier::Identifier;

/// The code an untyped failure reports.
///
/// One value, not a family. A code exists so a consumer can branch on it; a
/// per-message code invented at the call site cannot be branched on, so assigning one
/// everywhere would be decoration.
pub const FALLBACK_CODE: &str = "zup.internal";

/// How loudly zup is saying something.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The operation cannot do what it was asked.
    Error,
    /// The operation finished, and something about the result deserves attention.
    Warning,
    /// Something a person may want to know. Never fails anything.
    Notice,
}

impl Severity {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Notice => "notice",
        }
    }
}

/// One thing zup wants a developer to know.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// How loudly zup is saying it.
    pub severity: Severity,
    /// A stable identifier, e.g. `zup.manifest.unknown_target`.
    pub code: Identifier,
    /// The one-sentence statement of what is wrong.
    pub message: String,
    /// Where it is wrong, when zup knows.
    pub source: Option<DiagnosticSource>,
    /// The line a developer can act on.
    pub help: Option<String>,
}

impl Diagnostic {
    /// An error with a code and a message, and nothing else.
    ///
    /// The code is `&'static str` on purpose. A code is something a consumer matches
    /// on, so it has to be a name somebody chose deliberately and can grep for - not a
    /// formatted string assembled at the call site, and not a Rust type or module name
    /// that a reorganization would rename out from under every integration.
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    /// A finding the operation survived.
    ///
    /// For something a person must know about and a pipeline must not fail on: a
    /// project that is valid but has a problem, rather than one that could not be read.
    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    /// A diagnostic at a chosen level.
    pub fn new(severity: Severity, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity,
            code: Identifier::fixed(code),
            message: message.into(),
            source: None,
            help: None,
        }
    }

    /// The same diagnostic with a remedy attached.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// The same diagnostic pointed at a file.
    pub fn in_file(mut self, file: impl Into<String>) -> Self {
        self.source = Some(DiagnosticSource {
            file: file.into(),
            start_line: None,
            start_column: None,
            end_line: None,
            end_column: None,
        });
        self
    }

    /// A one-line identity, for deduplicating a streamed diagnostic against the same
    /// diagnostic in the final result.
    ///
    /// Two diagnostics with the same code and message at the same place are the same
    /// finding, and a consumer that annotates both has told the reader a fact twice.
    /// The identity deliberately includes the location: the same message about two
    /// different lines is two problems.
    pub fn identity(&self) -> String {
        let location = self
            .source
            .as_ref()
            .map(|source| {
                format!(
                    "@{}:{}:{}",
                    source.file,
                    source.start_line.unwrap_or(0),
                    source.start_column.unwrap_or(0)
                )
            })
            .unwrap_or_default();
        format!("{}\u{1f}{}\u{1f}{location}", self.code, self.message)
    }
}

/// Where a diagnostic points.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSource {
    /// The file, as the diagnostic names it. Usually project-relative.
    pub file: String,
    /// 1-based.
    pub start_line: Option<u32>,
    /// 1-based.
    pub start_column: Option<u32>,
    /// 1-based, inclusive.
    pub end_line: Option<u32>,
    /// 1-based, inclusive.
    pub end_column: Option<u32>,
}

impl DiagnosticSource {
    /// A file with no position in it, which is what a whole-file complaint has.
    pub fn file(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            start_line: None,
            start_column: None,
            end_line: None,
            end_column: None,
        }
    }

    /// The same location, pointed at a line and column.
    pub fn at(file: impl Into<String>, line: u32, column: u32) -> Self {
        Self {
            file: file.into(),
            start_line: Some(line),
            start_column: Some(column),
            end_line: None,
            end_column: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dedup key the Action uses to avoid annotating a streamed diagnostic and
    /// then the same diagnostic again from the final result.
    #[test]
    fn identity_separates_the_same_message_at_two_places() {
        let first =
            Diagnostic::error("zup.manifest.unknown_target", "no such target").in_file("zup.toml");
        let mut second = first.clone();
        second.source = Some(DiagnosticSource::at("zup.toml", 12, 3));
        let third =
            Diagnostic::error("zup.manifest.unknown_target", "no such target").in_file("zup.toml");
        assert_eq!(first.identity(), third.identity());
        assert_ne!(first.identity(), second.identity());
    }
}
