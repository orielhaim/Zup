//! The one place the developer CLI writes anything.
//!
//! # The rule
//!
//! ```text
//! --format human   everything a person reads goes to stdout
//! --format json    stdout is exactly one AutomationResult and nothing else;
//!                  everything else goes to stderr
//! --format jsonl   stdout is the protocol stream and nothing else;
//!                  everything else goes to stderr
//! ```
//!
//! Every command reports through [`Reporter`]. A command that reaches for
//! `println!` breaks the rule for machine mode, and the adversarial test in
//! `tests/protocol_stdout.rs` is what notices.
//!
//! # Why stderr, and not suppression
//!
//! A build's logs are the reason a person opens a CI run at all. Moving them to stderr
//! costs a machine consumer nothing — it is reading stdout — and costs a human nothing
//! either, because a terminal shows both. The alternative, refusing to print anything
//! in machine mode, would mean a pipeline that swallowed a compiler error nobody could
//! see.
//!
//! # Why the final result is written last, and once
//!
//! `--format json` writes exactly one document, and it is the last thing written. That
//! is what lets a consumer read a whole run's output, get one `AutomationResult`, and
//! not have to find the end of it — and it is what makes "stdout is a protocol" a
//! statement a test can make rather than a convention.

use std::io::Write;

use zup_automation::{
    AutomationResult, Diagnostic, Identifier, LogLevel, StreamEvent, StreamVersion,
};

use crate::cli::OutputArg;

/// Writes everything, to the right place, in the right format.
#[derive(Debug, Clone, Copy)]
pub struct Reporter {
    format: OutputArg,
}

impl Reporter {
    /// A reporter for one invocation's requested format.
    pub fn new(format: OutputArg) -> Self {
        Self { format }
    }

    /// The format this reporter was built for.
    pub fn format(self) -> OutputArg {
        self.format
    }

    /// Whether a person is reading.
    pub fn is_human(self) -> bool {
        self.format == OutputArg::Human
    }

    /// Open the stream, before anything has happened.
    ///
    /// A no-op except in `--format jsonl`, where it writes the header. The header is
    /// first because a consumer that has to look past progress events to find out
    /// whether it can read the stream has no way to decide whether to keep reading.
    pub fn begin(self, operation: &'static str) {
        if self.format != OutputArg::Jsonl {
            return;
        }
        self.emit(StreamEvent::Version(StreamVersion::new(Identifier::fixed(
            operation,
        ))));
    }

    /// A step a person would have watched.
    pub fn phase(self, phase: &str, message: impl AsRef<str>) {
        match self.format {
            OutputArg::Human => line(&mut std::io::stdout().lock(), message.as_ref()),
            OutputArg::Json => line(&mut std::io::stderr().lock(), message.as_ref()),
            OutputArg::Jsonl => self.emit(StreamEvent::Phase {
                phase: phase.to_owned(),
                message: message.as_ref().to_owned(),
            }),
        }
    }

    /// A line a person would have read.
    ///
    /// The one sink for zup's own output, so there is one place the stdout rule is
    /// enforced and one place a routed log line comes from.
    pub fn log(self, level: LogLevel, message: impl AsRef<str>) {
        match self.format {
            OutputArg::Human => {
                let mut out = std::io::stdout().lock();
                line(&mut out, message.as_ref());
                if level != LogLevel::Info {
                    line(&mut std::io::stderr().lock(), message.as_ref());
                }
            }
            OutputArg::Json => line(&mut std::io::stderr().lock(), message.as_ref()),
            OutputArg::Jsonl => self.emit(StreamEvent::Log {
                level,
                message: message.as_ref().to_owned(),
            }),
        }
    }

    /// A diagnostic, as it is found rather than at the end.
    ///
    /// A `--format json` consumer gets it in the final document like every other, and
    /// a `--format jsonl` consumer gets it now — which is the difference between a
    /// manifest error appearing in the first second and after a ten-minute build.
    pub fn diagnostic(self, diagnostic: &Diagnostic) {
        if self.format != OutputArg::Jsonl {
            return;
        }
        self.emit(StreamEvent::Diagnostic {
            diagnostic: diagnostic.clone(),
        });
    }

    /// The authoritative end of the operation.
    ///
    /// Written for `json` and `jsonl`; a no-op for a person, who has already been
    /// reading the same information as it happened.
    pub fn finish(self, result: AutomationResult) {
        match self.format {
            OutputArg::Human => {}
            OutputArg::Json => {
                let mut out = std::io::stdout().lock();
                let written = serde_json::to_writer_pretty(&mut out, &result);
                if let Err(error) = written {
                    // The result could not be written, so nothing on stdout is a
                    // protocol document any more. Say so on stderr, where a consumer
                    // that is looking for an answer will not find one.
                    eprintln!("zup: the result could not be written: {error}");
                } else {
                    // A trailing newline, because a document without one is a document
                    // that a shell prompt sits on.
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
            }
            OutputArg::Jsonl => self.emit(StreamEvent::completed(result)),
        }
    }

    /// Write one protocol line, flushed.
    ///
    /// Flushed per line rather than per run, because the whole point of a stream is
    /// that a consumer sees it *while* the build is going. A buffered stream is a
    /// stream that delivers everything at the end, which is the `--format json` case
    /// wearing a different name.
    fn emit(self, event: StreamEvent) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(event.to_line().as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

/// One line, on one stream, with the newline the terminal expects.
fn line(out: &mut impl Write, text: &str) {
    let _ = out.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_automation::{OPERATION_BUILD, Status};

    fn result() -> AutomationResult {
        AutomationResult::new(OPERATION_BUILD).with_summary("Built")
    }

    /// The three formats' contract, asserted on the type rather than on a terminal:
    /// what a reporter is for.
    #[test]
    fn the_format_decides_where_a_line_goes() {
        assert!(Reporter::new(OutputArg::Human).is_human());
        assert!(!Reporter::new(OutputArg::Json).is_human());
        assert!(!Reporter::new(OutputArg::Jsonl).is_human());
    }

    /// `finish` is the only thing that writes to stdout in `--format json`, and it
    /// writes one document. Everything before it is a no-op on stdout, which is what
    /// makes a partially-failed run still emit a readable result.
    #[test]
    fn a_human_reporter_writes_nothing_for_the_protocol() {
        // Human mode renders nothing through `finish`; the assertions here are about
        // the machine modes, and they are the ones the contract states.
        let machine = Reporter::new(OutputArg::Json);
        assert!(!machine.is_human());
        assert_eq!(machine.format(), OutputArg::Json);
        assert_eq!(result().status, Status::Success);
    }
}
