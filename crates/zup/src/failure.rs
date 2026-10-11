use std::fmt;

use zup_automation::{
    AutomationResult, Diagnostic, DiagnosticSource, FALLBACK_CODE, Identifier, LogLevel, Severity,
    StreamEvent, StreamVersion,
};

#[derive(Debug)]
pub struct Coded {
    severity: Severity,
    code: Identifier,
    message: String,
    help: Option<String>,
}

impl Coded {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self::identified(Identifier::fixed(code), message)
    }

    pub fn identified(code: Identifier, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            help: None,
        }
    }

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

pub fn error(code: &'static str, message: impl Into<String>) -> miette::Report {
    miette::Report::new(Coded::error(code, message))
}

pub fn error_with_help(
    code: &'static str,
    message: impl Into<String>,
    help: impl Into<String>,
) -> miette::Report {
    miette::Report::new(Coded::error(code, message).with_help(help))
}

pub fn identified(code: Identifier, message: impl Into<String>) -> miette::Report {
    miette::Report::new(Coded::identified(code, message))
}

/// Never fails and never guesses a code: a code that is not a valid identifier becomes
pub fn diagnostic(report: &miette::Report) -> Diagnostic {
    let source: &dyn miette::Diagnostic = report.as_ref();
    let mut diagnostic = Diagnostic {
        severity: source.severity().map_or(Severity::Error, wire_severity),
        code: code_of(source),
        message: source.to_string(),
        source: source_span(source),
        help: source.help().map(|help| help.to_string()),
    };
    if let Some(coded) = find_coded(report) {
        diagnostic.code = coded.code.clone();
        diagnostic.severity = coded.severity;
        if diagnostic.help.is_none() {
            diagnostic.help = coded.help.clone();
        }
    }
    diagnostic
}

pub fn failure(operation: &str, report: &miette::Report, summary: impl Into<String>) -> Diagnostic {
    let mut diagnostic = diagnostic(report);
    if diagnostic.code.is(FALLBACK_CODE) {
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

fn wire_severity(severity: miette::Severity) -> Severity {
    match severity {
        miette::Severity::Error => Severity::Error,
        miette::Severity::Warning => Severity::Warning,
        miette::Severity::Advice => Severity::Notice,
    }
}

fn code_of(source: &dyn miette::Diagnostic) -> Identifier {
    source
        .code()
        .and_then(|code| Identifier::parse(&code.to_string()).ok())
        .unwrap_or_else(|| Identifier::fixed(FALLBACK_CODE))
}

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

/// rather than counting newlines is what makes a span in a file the error never opened
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

    /// Context added on the way out must not erase the identity decided at the bottom
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

    #[test]
    fn a_diagnostic_with_a_span_points_at_a_line_and_column() {
        let diagnostic = diagnostic(&spanned("zup.toml", "schema = 1\napp = 2\n", 13));
        let location = diagnostic.source.expect("a location");
        assert_eq!(location.file, "zup.toml");
        assert_eq!(location.start_line, Some(2));
        assert_eq!(location.start_column, Some(3));
    }

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

use std::io::Write;

use crate::cli::OutputArg;

#[derive(Debug, Clone, Copy)]
pub struct Reporter {
    format: OutputArg,
}

impl Reporter {
    pub fn new(format: OutputArg) -> Self {
        Self { format }
    }

    pub fn format(self) -> OutputArg {
        self.format
    }

    pub fn is_human(self) -> bool {
        self.format == OutputArg::Human
    }

    pub fn begin(self, operation: &'static str) {
        if self.format != OutputArg::Jsonl {
            return;
        }
        self.emit(StreamEvent::Version(StreamVersion::new(Identifier::fixed(
            operation,
        ))));
    }

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

    pub fn diagnostic(self, diagnostic: &Diagnostic) {
        if self.format != OutputArg::Jsonl {
            return;
        }
        self.emit(StreamEvent::Diagnostic {
            diagnostic: diagnostic.clone(),
        });
    }

    pub fn finish(self, result: AutomationResult) {
        match self.format {
            OutputArg::Human => {}
            OutputArg::Json => {
                let mut out = std::io::stdout().lock();
                let written = serde_json::to_writer_pretty(&mut out, &result);
                if let Err(error) = written {
                    eprintln!("zup: the result could not be written: {error}");
                } else {
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
            }
            OutputArg::Jsonl => self.emit(StreamEvent::completed(result)),
        }
    }

    fn emit(self, event: StreamEvent) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(event.to_line().as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

fn line(out: &mut impl Write, text: &str) {
    let _ = out.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
}
