//! Turning what a command failed with into something an integration can act on.
//!
//! # miette owns the error, zup owns the identity
//!
//! `miette` renders diagnostics beautifully and is not a wire contract: its JSON
//! report handler is a rendering of a Rust error type, and it changes when the error
//! type, the crate layout or the handler's own defaults change. So it stays exactly
//! where it is — the thing that produces a good terminal message and a source span —
//! and this module reads the four things zup promises on the wire out of it:
//!
//! ```text
//! code       a stable zup.* identifier
//! severity   error, warning or notice
//! help       the line a developer can act on
//! source     the file and, when miette has one, the line and column
//! ```
//!
//! An error that carries none of them still becomes a real diagnostic with a real
//! message, under [`FALLBACK_CODE`]. That is the point: a consumer that matches on
//! codes gets silence for the untyped ones, and a consumer that shows messages gets
//! every failure.
//!
//! # Where a code comes from
//!
//! From [`Coded`], and only from [`Coded`]. A code is a published identifier, so it is
//! a `&'static str` chosen at the call site where somebody decided what the failure
//! *means* — not a formatted string, not a `Debug` rendering, and never a Rust type or
//! module name. `zup_manifest::UnknownTarget` is not a code: it is renamed the day
//! somebody reorganises a module, and every integration that matched on it would break
//! on a change no operator could see.

use std::fmt;

use zup_automation::{Diagnostic, DiagnosticSource, FALLBACK_CODE, Identifier, Severity};

/// A failure that knows what it is.
///
/// Implements `miette::Diagnostic` so it flows through the same `?` as everything
/// else, and carries the three things the protocol promises on top of the message.
#[derive(Debug)]
pub struct Coded {
    severity: Severity,
    code: Identifier,
    message: String,
    help: Option<String>,
}

impl Coded {
    /// An error with a code.
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self::identified(Identifier::fixed(code), message)
    }

    /// An error with a code that was decided at run time, rather than at a call site.
    ///
    /// For the one place a code comes from a document rather than from a literal: the
    /// exit path of a command whose result already carries one.
    pub fn identified(code: Identifier, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            help: None,
        }
    }

    /// The same failure with the line a developer can act on.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

impl fmt::Display for Coded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

impl miette::Diagnostic for Coded {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(self.code.to_string()))
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
            Severity::Notice => miette::Severity::Advice,
        })
    }

    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.help
            .as_ref()
            .map(|help| Box::new(help.clone()) as Box<dyn fmt::Display>)
    }
}

/// A coded error, as the value a command returns.
pub fn error(code: &'static str, message: impl Into<String>) -> miette::Report {
    miette::Report::new(Coded::error(code, message))
}

/// A coded error with a remedy attached.
pub fn error_with_help(
    code: &'static str,
    message: impl Into<String>,
    help: impl Into<String>,
) -> miette::Report {
    miette::Report::new(Coded::error(code, message).with_help(help))
}

/// A coded error whose code was decided at run time.
pub fn identified(code: Identifier, message: impl Into<String>) -> miette::Report {
    miette::Report::new(Coded::identified(code, message))
}

/// The wire diagnostic for anything a command failed with.
///
/// Never fails and never guesses a code: a code that is not a valid identifier becomes
/// [`FALLBACK_CODE`], because a code a consumer cannot match on is worse than one that
/// says plainly that nothing is known.
pub fn diagnostic(report: &miette::Report) -> Diagnostic {
    let source: &dyn miette::Diagnostic = report.as_ref();
    let mut diagnostic = Diagnostic {
        severity: source.severity().map_or(Severity::Error, wire_severity),
        code: code_of(source),
        message: source.to_string(),
        source: source_span(source),
        help: source.help().map(|help| help.to_string()),
    };
    // A report wrapping a coded error, several layers down, is still a coded error.
    if let Some(coded) = find_coded(report) {
        diagnostic.code = coded.code.clone();
        diagnostic.severity = coded.severity;
        if diagnostic.help.is_none() {
            diagnostic.help = coded.help.clone();
        }
    }
    diagnostic
}

/// A failure result for an operation that got far enough to be named.
pub fn failure(operation: &str, report: &miette::Report, summary: impl Into<String>) -> Diagnostic {
    let mut diagnostic = diagnostic(report);
    if diagnostic.code.is(FALLBACK_CODE) {
        // The operation is a better identity than `zup.internal` for a failure that
        // happened before any of zup's own checks ran: it names where to look.
        diagnostic.code = Identifier::fixed(match operation {
            "build" => "zup.build.failed",
            "check" => "zup.check.failed",
            "doctor" => "zup.doctor.failed",
            "plan" => "zup.plan.failed",
            "publish.stage" => "zup.publish.stage_failed",
            "publish.github" => "zup.publish.failed",
            "sign.prepare" => "zup.signing.prepare_failed",
            "sign.verify" => "zup.signing.verify_failed",
            "toolchain.install" => "zup.toolchain.install_failed",
            "toolchain.status" => "zup.toolchain.status_failed",
            "toolchain.clean" => "zup.toolchain.clean_failed",
            "artifact.inspect" => "zup.artifact.invalid",
            _ => FALLBACK_CODE,
        });
    }
    let _ = summary;
    diagnostic
}

/// miette's levels as the protocol's three.
///
/// `Advice` is a notice rather than a warning: miette uses it for a suggestion, and a
/// suggestion turned into a warning would be something every pipeline has to triage.
fn wire_severity(severity: miette::Severity) -> Severity {
    match severity {
        miette::Severity::Error => Severity::Error,
        miette::Severity::Warning => Severity::Warning,
        miette::Severity::Advice => Severity::Notice,
    }
}

/// The code a `miette` diagnostic carries, or the fallback.
fn code_of(source: &dyn miette::Diagnostic) -> Identifier {
    source
        .code()
        .and_then(|code| Identifier::parse(&code.to_string()).ok())
        .unwrap_or_else(|| Identifier::fixed(FALLBACK_CODE))
}

/// The first [`Coded`] in a report's chain, which is where a wrapped one keeps its
/// identity.
///
/// A `miette` report is a chain because `wrap_err` produces one, and a command that
/// adds context at each layer is doing the right thing — the identity is what was
/// decided at the bottom of it, not at the top.
fn find_coded(report: &miette::Report) -> Option<&Coded> {
    if let Some(coded) = report.downcast_ref::<Coded>() {
        return Some(coded);
    }
    let mut current: Option<&(dyn std::error::Error + 'static)> =
        std::error::Error::source(report.as_ref() as &(dyn std::error::Error + 'static));
    while let Some(error) = current {
        if let Some(coded) = error.downcast_ref::<Coded>() {
            return Some(coded);
        }
        current = error.source();
    }
    None
}

/// Where a diagnostic points, when miette knows.
///
/// `read_span` is how miette answers "which line is that offset on", and asking it
/// rather than counting newlines is what makes a span in a file the error never opened
/// land on the right line — and it is where the file's name comes from, because a
/// `SourceCode` has no name of its own and only the span it produced does. A
/// diagnostic with no source code, no label, or a span it cannot read is a diagnostic
/// with no location, which is a normal outcome rather than a failure to report.
fn source_span(source: &dyn miette::Diagnostic) -> Option<DiagnosticSource> {
    let code = source.source_code()?;
    let label = source.labels()?.next()?;
    let contents = code.read_span(label.inner(), 0, 0).ok()?;
    let file = contents.name()?.to_owned();
    let start_line = contents.line() as u32 + 1;
    let start_column = contents.column() as u32 + 1;
    if contents.line_count() <= 1 {
        return Some(DiagnosticSource {
            file,
            start_line: Some(start_line),
            start_column: Some(start_column),
            end_line: Some(start_line),
            end_column: Some(start_column + label.len().saturating_sub(1) as u32),
        });
    }
    Some(DiagnosticSource {
        file,
        start_line: Some(start_line),
        start_column: Some(start_column),
        end_line: Some(start_line + contents.line_count() as u32 - 1),
        end_column: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coded_error_keeps_its_code_severity_and_help() {
        let report = error_with_help(
            "zup.manifest.unknown_target",
            "`linux` is not among the selected targets",
            "declare it under [build.targets.linux]",
        );
        let diagnostic = diagnostic(&report);
        assert!(diagnostic.code.is("zup.manifest.unknown_target"));
        assert_eq!(diagnostic.severity, Severity::Error);
        assert!(
            diagnostic.message.contains("linux"),
            "{}",
            diagnostic.message
        );
        assert_eq!(
            diagnostic.help.as_deref(),
            Some("declare it under [build.targets.linux]")
        );
    }

    /// The untyped path, which is most of them: an error from a domain crate wrapped in
    /// context still reaches a consumer as a diagnostic with a message.
    #[test]
    fn an_untyped_error_becomes_a_diagnostic_rather_than_nothing() {
        let report = miette::miette!("`{}` could not be read", "dist/zup-release.json")
            .wrap_err("publishing v1.4.0");
        let diagnostic = diagnostic(&report);
        assert!(diagnostic.code.is(FALLBACK_CODE));
        assert_eq!(diagnostic.severity, Severity::Error);
        assert!(!diagnostic.message.is_empty());
    }

    /// Context added on the way out must not erase the identity decided at the bottom
    /// of it, or every wrapped error would be indistinguishable from an untyped one.
    #[test]
    fn a_wrapped_code_survives_the_wrapping() {
        let inner = error(
            "zup.toolchain.component_mismatch",
            "the runtime is from zup 9.9.9",
        );
        let report = inner.wrap_err("composing Acme-Setup.exe");
        assert!(
            diagnostic(&report)
                .code
                .is("zup.toolchain.component_mismatch")
        );
    }

    /// The per-operation fallback exists for the failures that happen before any of
    /// zup's own checks can run, where `zup.internal` would name nothing at all.
    #[test]
    fn an_untyped_failure_is_named_after_the_operation() {
        let report = miette::miette!("no `zup.toml` here");
        for (operation, code) in [
            ("build", "zup.build.failed"),
            ("publish.github", "zup.publish.failed"),
            ("sign.verify", "zup.signing.verify_failed"),
            ("artifact.inspect", "zup.artifact.invalid"),
        ] {
            assert!(failure(operation, &report, "").code.is(code));
        }
    }

    /// A location, when the error carries one. A manifest error carries a name, a span
    /// and a label, and the line and column are what a CI annotation hangs off. The span
    /// is on the second `p` of `app`, so the answer is line 2, column 3.
    #[test]
    fn a_diagnostic_with_a_span_points_at_a_line_and_column() {
        let diagnostic = diagnostic(&spanned("zup.toml", "schema = 1\napp = 2\n", 13));
        let location = diagnostic.source.expect("a location");
        assert_eq!(location.file, "zup.toml");
        assert_eq!(location.start_line, Some(2));
        assert_eq!(location.start_column, Some(3));
    }

    /// The shape the manifest layer produces: a code, a message, a named source and a
    /// label on the offending bytes.
    #[derive(Debug)]
    struct Spanned {
        inner: Coded,
        source: miette::NamedSource<String>,
        offset: usize,
    }

    impl fmt::Display for Spanned {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.inner.fmt(f)
        }
    }

    impl std::error::Error for Spanned {}

    impl miette::Diagnostic for Spanned {
        fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
            self.inner.code()
        }

        fn source_code(&self) -> Option<&dyn miette::SourceCode> {
            Some(&self.source)
        }

        fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
            Some(Box::new(std::iter::once(miette::LabeledSpan::at(
                self.offset,
                "here",
            ))))
        }
    }

    fn spanned(file: &str, contents: &str, offset: usize) -> miette::Report {
        miette::Report::new(Spanned {
            inner: Coded::error("zup.manifest.invalid", "`app` must have a name"),
            source: miette::NamedSource::new(file, contents.to_owned()),
            offset,
        })
    }
}
