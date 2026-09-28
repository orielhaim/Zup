//! The live stream `--format jsonl` writes.
//!
//! A long build is ten minutes of nothing visible, and a CI consumer that only sees
//! the final document learns about a manifest error at minute ten. So `--format jsonl`
//! writes one JSON object per line as the operation proceeds, and the *last* line
//! carries the same [`AutomationResult`] the `--format json` document would have been.
//!
//! That last line is the whole design. A consumer gets a live UX from the events before
//! it and one authoritative snapshot from the final one, and it never has to
//! reconstruct final state by folding events — which is the thing that breaks when a
//! producer adds an event, drops one, or emits them in a different order.
//!
//! # The vocabulary
//!
//! ```text
//! version      always first, always exactly one
//! phase        a step began or ended
//! progress     a counter moved
//! diagnostic   something a reader should know, now rather than at the end
//! artifact     a file was produced
//! publication  a release went somewhere
//! log          human output routed out of stdout
//! completed    always last, always exactly one
//! ```
//!
//! Eight messages, for the whole product. A message per internal function would be a
//! protocol that changes every time a function is renamed, and a consumer that had to
//! understand all of them to read the stream would break on any of them.
//!
//! # Forward compatibility
//!
//! [`StreamEvent::Unknown`] is what a decoder gets for a `type` it does not know. The
//! producer side is an enum because a producer should not be able to emit a type it has
//! no constructor for; the consumer side is a decoder that must not be a closed enum,
//! because that is what would make a minor bump a breaking change for everyone already
//! running.
//!
//! The stream has no timestamps. Nothing in a build report needs a wall clock, and one
//! would make every fixture non-deterministic and every diff a lie. Elapsed time is a
//! measurement, not a fact about the document; a consumer that wants it can time the
//! process.

use serde::{Deserialize, Serialize};

use crate::artifact::Artifact;
use crate::diagnostic::Diagnostic;
use crate::identifier::Identifier;
use crate::publication::Publication;
use crate::result::AutomationResult;
use crate::version::PROTOCOL;

/// The first line of every stream, and the only place the tool version appears.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamVersion {
    /// The contract this stream is written to.
    pub protocol: crate::version::ProtocolVersion,
    /// The zup release producing it.
    pub zup: String,
    /// The operation that is about to run.
    pub operation: Identifier,
}

impl StreamVersion {
    /// The header for one operation.
    pub fn new(operation: Identifier) -> Self {
        Self {
            protocol: PROTOCOL,
            zup: crate::ZUP_VERSION.to_owned(),
            operation,
        }
    }
}

/// How a routed human line is marked.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

impl LogLevel {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One line of a stream.
#[cfg_attr(feature = "bindings", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The first line. Names the protocol, the tool and the operation.
    Version(StreamVersion),
    /// A step began.
    Phase {
        /// A short, stable name for the step.
        phase: String,
        /// The sentence a person would have read.
        message: String,
    },
    /// A counter moved.
    Progress {
        /// Items finished.
        completed: u32,
        /// Items in total. Always greater than zero: a progress event with no total has
        /// nothing to be a fraction of, and a consumer that rendered it would be
        /// showing a number that means nothing.
        total: u32,
        /// The label a person would have read.
        label: String,
    },
    /// Something a reader should know now rather than at the end.
    Diagnostic { diagnostic: Diagnostic },
    /// A file was produced.
    Artifact { artifact: Artifact },
    /// A release went somewhere.
    Publication { publication: Publication },
    /// Human output that would have gone to a terminal.
    ///
    /// Exists so a build's logs can be *shown* without being *suppressed*: in
    /// `--format jsonl` the child's and zup's own human lines come out here, and a
    /// consumer that ignores them still has a complete stream.
    Log { level: LogLevel, message: String },
    /// The last line. The authoritative result.
    ///
    /// Boxed because the result is three orders of magnitude larger than any other
    /// event, and a stream that carries thousands of progress lines would otherwise
    /// pay for it on every line. Invisible on the wire.
    Completed { result: Box<AutomationResult> },
    /// A message type this build does not know.
    ///
    /// Decode-only. It exists so a consumer inside the same major can keep reading a
    /// stream a newer zup produced; serializing it would be zup claiming to have sent
    /// a message it did not, so nothing constructs it — which is also why it is not in
    /// the generated TypeScript, where a union member that can never arrive would be a
    /// case every consumer had to write.
    #[cfg_attr(feature = "bindings", schemars(skip))]
    #[cfg_attr(feature = "bindings", ts(skip))]
    #[serde(other)]
    Unknown,
}

impl StreamEvent {
    /// The discriminator, for a consumer that dispatches on it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Version(_) => "version",
            Self::Phase { .. } => "phase",
            Self::Progress { .. } => "progress",
            Self::Diagnostic { .. } => "diagnostic",
            Self::Artifact { .. } => "artifact",
            Self::Publication { .. } => "publication",
            Self::Log { .. } => "log",
            Self::Completed { .. } => "completed",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this is the line that ends the stream.
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// The result this line carries, if it is the last one.
    pub fn result(&self) -> Option<&AutomationResult> {
        match self {
            Self::Completed { result } => Some(result),
            _ => None,
        }
    }

    /// The last line of a stream, which is the result the stream was for.
    pub fn completed(result: AutomationResult) -> Self {
        Self::Completed {
            result: Box::new(result),
        }
    }

    /// The line as one line of JSON, without its newline.
    ///
    /// Hand-rolled rather than a `serde_json::to_string` per event plus a wrapper,
    /// because this is on the hot path of a build that may emit thousands of events and
    /// because a serializer that pretty-printed would silently turn a stream into a
    /// file.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|error| {
            // A stream event that cannot be serialized is a bug in the event, not a
            // condition a caller can do anything about. Emitting a log line keeps the
            // stream well-formed instead of tearing a hole in it, and the message says
            // which event so the bug is findable.
            let fallback = StreamEvent::Log {
                level: LogLevel::Error,
                message: format!("a `{}` event could not be serialized: {error}", self.kind()),
            };
            serde_json::to_string(&fallback).unwrap_or_else(|_| {
                String::from(
                    "{\"type\":\"log\",\"level\":\"error\",\"message\":\"unserializable\"}",
                )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Diagnostic;
    use crate::operation::OPERATION_BUILD;

    /// A stream is well-formed: one version line, then anything, then one completed
    /// line. The check is on the shape, not on the order of the middle, because a
    /// producer that reorders progress is making no promise.
    #[test]
    fn a_stream_begins_with_a_version_and_ends_with_a_result() {
        let mut lines = vec![StreamEvent::Version(StreamVersion::new(Identifier::fixed(
            OPERATION_BUILD,
        )))];
        lines.push(StreamEvent::Phase {
            phase: "compose".to_owned(),
            message: "Composing artifact windows-x64".to_owned(),
        });
        lines.push(StreamEvent::Progress {
            completed: 1,
            total: 4,
            label: "4 objects".to_owned(),
        });
        lines.push(StreamEvent::Diagnostic {
            diagnostic: Diagnostic::error("zup.build.digest_mismatch", "changed"),
        });
        let result = AutomationResult::new(OPERATION_BUILD).with_summary("Built");
        lines.push(StreamEvent::completed(result.clone()));

        let stream = lines
            .iter()
            .map(StreamEvent::to_line)
            .collect::<Vec<_>>()
            .join("\n");
        let decoded = stream
            .lines()
            .map(|line| serde_json::from_str::<StreamEvent>(line).expect("a stream line"))
            .collect::<Vec<_>>();
        assert!(matches!(decoded.first(), Some(StreamEvent::Version(_))));
        assert_eq!(decoded.first().map(StreamEvent::kind), Some("version"));
        assert_eq!(decoded.last().and_then(StreamEvent::result), Some(&result));
        assert!(decoded.last().map(StreamEvent::is_final) == Some(true));
        // One object per line, and no line carries a pretty-print.
        assert!(stream.lines().all(|line| !line.contains('\n')));
        assert!(stream.contains(r#""protocol":"1.0""#), "{stream}");
    }

    /// A diagnostic travels as an object rather than being flattened into the line, so
    /// the same `Diagnostic` type serves the stream and the final result and there is
    /// only one place a consumer learns its shape.
    #[test]
    fn a_streamed_diagnostic_is_the_same_shape_as_the_final_one() {
        let diagnostic =
            Diagnostic::error("zup.manifest.unknown_target", "no such target").in_file("zup.toml");
        let line = StreamEvent::Diagnostic {
            diagnostic: diagnostic.clone(),
        }
        .to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "diagnostic");
        let decoded: StreamEvent = serde_json::from_str(&line).unwrap();
        let StreamEvent::Diagnostic { diagnostic: back } = decoded else {
            panic!("a diagnostic line");
        };
        assert_eq!(back, diagnostic);
        assert_eq!(back.identity(), diagnostic.identity());
    }
}
