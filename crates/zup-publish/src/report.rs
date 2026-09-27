//! The structured result of a publication.
//!
//! A report is phases, each with steps. The phases are the ones a person reading
//! a CI log actually needs to be able to name: what was checked, what was sent,
//! what was proved, what became permanent. The steps are the facts. A JSON
//! consumer reads the same structure rather than scraping a terminal, which is
//! the only reason a `--format json` flag is worth having.

use serde::{Deserialize, Serialize};

use crate::format_bytes;
use crate::receipt::{Notice, PublishReceipt};

/// Version of the report shape.
pub const REPORT_SCHEMA: u32 = 1;

/// How one phase ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    /// Everything in the phase was done.
    Complete,
    /// The phase had nothing to do.
    Skipped,
    /// The phase stopped early.
    Failed,
}

impl PhaseStatus {
    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// How one step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// It happened and the result is as expected.
    Ok,
    /// It did not apply, or there was nothing to do.
    Info,
    /// It did not apply, and something should change.
    Warn,
    /// It failed.
    Fail,
}

impl StepStatus {
    /// The glyph a human report uses.
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Info => "-",
            Self::Warn => "!",
            Self::Fail => "✗",
        }
    }

    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepReport {
    pub label: String,
    pub status: StepStatus,
    /// A second line, for the reason a step warned or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The bytes this step moved, when it moved any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl StepReport {
    /// The size this step moved, rendered.
    pub fn size(&self) -> Option<String> {
        self.bytes.map(format_bytes)
    }
}

/// One stage of a publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseReport {
    /// The phase name, as a person would say it: `Uploading`, `Verifying`.
    pub name: String,
    pub status: PhaseStatus,
    pub steps: Vec<StepReport>,
}

/// Everything one publication attempt did, or would have done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishReport {
    pub schema: u32,
    /// The provider, e.g. `github`.
    pub provider: String,
    /// Where it went.
    pub subject: String,
    pub tag: String,
    pub version: String,
    /// Whether nothing was written.
    pub dry_run: bool,
    /// How many files the plan names.
    pub assets: usize,
    /// How many bytes they take.
    pub bytes: u64,
    pub phases: Vec<PhaseReport>,
    /// Facts about the publication that are not files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
    /// The receipt, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<PublishReceipt>,
}

impl PublishReport {
    /// Whether every phase completed.
    pub fn is_complete(&self) -> bool {
        self.phases.iter().all(|phase| {
            phase.status == PhaseStatus::Complete || phase.status == PhaseStatus::Skipped
        })
    }

    /// The steps that failed, across every phase.
    pub fn failures(&self) -> Vec<&StepReport> {
        self.phases
            .iter()
            .flat_map(|phase| phase.steps.iter())
            .filter(|step| step.status == StepStatus::Fail)
            .collect()
    }

    /// The report as a person reads it.
    ///
    /// Two columns of aligned labels, because a list of files of wildly different
    /// sizes is unreadable without them, and a phase header is worth a blank line
    /// because it is what a reader is scanning for.
    pub fn human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{:<15}{}\n", "Release", self.tag));
        out.push_str(&format!("{:<15}{}\n", "Repository", self.subject));
        if self.dry_run {
            out.push_str(&format!("{:<15}{}\n", "Mode", "dry run (nothing written)"));
        }
        out.push_str(&format!(
            "{:<15}{} files · {}\n",
            "Assets",
            self.assets,
            format_bytes(self.bytes)
        ));
        for phase in &self.phases {
            if phase.steps.is_empty() {
                continue;
            }
            let width = phase
                .steps
                .iter()
                .map(|step| step.label.chars().count())
                .max()
                .unwrap_or(0)
                .min(44);
            out.push('\n');
            out.push_str(&phase.name);
            out.push('\n');
            for step in &phase.steps {
                let label = truncate(&step.label, width);
                let size = step
                    .size()
                    .map(|size| format!("  {size:>10}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {} {label:<width$}{size}\n",
                    step.status.glyph(),
                    width = width
                ));
                if let Some(detail) = &step.detail {
                    for line in detail.lines() {
                        out.push_str(&format!("      {line}\n"));
                    }
                }
            }
        }
        if !self.notices.is_empty() {
            out.push('\n');
            out.push_str("Release integrity\n");
            let width = self
                .notices
                .iter()
                .map(|notice| notice.label.chars().count())
                .max()
                .unwrap_or(0);
            for notice in &self.notices {
                let glyph = match notice.status.as_str() {
                    "ok" => StepStatus::Ok,
                    "warn" => StepStatus::Warn,
                    _ => StepStatus::Info,
                };
                out.push_str(&format!(
                    "  {} {:<width$}{}\n",
                    glyph.glyph(),
                    notice.label,
                    notice
                        .detail
                        .as_deref()
                        .map(|detail| format!("  {detail}"))
                        .unwrap_or_default(),
                ));
            }
        }
        out
    }
}

fn truncate(label: &str, width: usize) -> String {
    if label.chars().count() <= width {
        return label.to_owned();
    }
    let mut out: String = label.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Builds a [`PublishReport`] one phase at a time.
///
/// A builder rather than a mutable report because the alternative is every
/// provider writing `report.phases.last_mut().expect("a phase is open")` at each
/// step, which is both ugly and a panic waiting for the first provider that
/// forgets to open a phase.
#[derive(Debug)]
pub struct ReportBuilder {
    report: PublishReport,
    open: Option<usize>,
}

impl ReportBuilder {
    /// Start a report for one publication.
    pub fn new(
        provider: impl Into<String>,
        subject: impl Into<String>,
        tag: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            report: PublishReport {
                schema: REPORT_SCHEMA,
                provider: provider.into(),
                subject: subject.into(),
                tag: tag.into(),
                version: version.into(),
                dry_run: false,
                assets: 0,
                bytes: 0,
                phases: Vec::new(),
                notices: Vec::new(),
                receipt: None,
            },
            open: None,
        }
    }

    /// Record that nothing will be written.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.report.dry_run = dry_run;
        self
    }

    /// Record what the plan holds.
    pub fn plan(&mut self, assets: usize, bytes: u64) {
        self.report.assets = assets;
        self.report.bytes = bytes;
    }

    /// Open a phase, and make it the one steps land in.
    pub fn phase(&mut self, name: impl Into<String>) -> &mut Self {
        self.report.phases.push(PhaseReport {
            name: name.into(),
            status: PhaseStatus::Complete,
            steps: Vec::new(),
        });
        self.open = Some(self.report.phases.len() - 1);
        self
    }

    /// Mark the open phase as one that had nothing to do.
    pub fn skip(&mut self) {
        if let Some(index) = self.open
            && let Some(phase) = self.report.phases.get_mut(index)
        {
            phase.status = PhaseStatus::Skipped;
        }
    }

    /// Mark the open phase as stopped early.
    pub fn fail(&mut self) {
        if let Some(index) = self.open
            && let Some(phase) = self.report.phases.get_mut(index)
        {
            phase.status = PhaseStatus::Failed;
        }
    }

    /// A step that happened.
    pub fn ok(&mut self, label: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Ok, None, None)
    }

    /// A step that did not apply.
    pub fn info(&mut self, label: impl Into<String>, detail: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Info, Some(detail.into()), None)
    }

    /// A step that moved bytes.
    pub fn sized(&mut self, label: impl Into<String>, bytes: u64) -> &mut Self {
        self.step(label, StepStatus::Ok, None, Some(bytes))
    }

    /// A step that did not happen and something should change.
    pub fn warn(&mut self, label: impl Into<String>, detail: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Warn, Some(detail.into()), None)
    }

    /// A step that failed.
    pub fn failed(&mut self, label: impl Into<String>, detail: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Fail, Some(detail.into()), None)
    }

    /// Add a step to the open phase.
    pub fn step(
        &mut self,
        label: impl Into<String>,
        status: StepStatus,
        detail: Option<String>,
        bytes: Option<u64>,
    ) -> &mut Self {
        let Some(index) = self.open else {
            return self;
        };
        if let Some(phase) = self.report.phases.get_mut(index) {
            phase.steps.push(StepReport {
                label: label.into(),
                status,
                detail,
                bytes,
            });
            if status == StepStatus::Fail {
                phase.status = PhaseStatus::Failed;
            }
        }
        self
    }

    /// Add an integrity notice.
    pub fn notice(&mut self, notice: Notice) -> &mut Self {
        self.report.notices.push(notice);
        self
    }

    /// Attach the receipt.
    pub fn receipt(&mut self, receipt: PublishReceipt) -> &mut Self {
        self.report.receipt = Some(receipt);
        self
    }

    /// The phases opened so far.
    pub fn phases(&self) -> &[PhaseReport] {
        &self.report.phases
    }

    /// The report.
    pub fn finish(self) -> PublishReport {
        self.report
    }
}
