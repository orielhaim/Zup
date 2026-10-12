use serde::{Deserialize, Serialize};

use crate::format_bytes;
use crate::receipt::{Notice, PublishReceipt};

pub const REPORT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Complete,
    Skipped,
    Failed,
}

impl PhaseStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Ok,
    Info,
    Warn,
    Fail,
}

impl StepStatus {
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Info => "-",
            Self::Warn => "!",
            Self::Fail => "✗",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepReport {
    pub label: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl StepReport {
    pub fn size(&self) -> Option<String> {
        self.bytes.map(format_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseReport {
    pub name: String,
    pub status: PhaseStatus,
    pub steps: Vec<StepReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishReport {
    pub schema: u32,
    pub provider: String,
    pub subject: String,
    pub tag: String,
    pub version: String,
    pub dry_run: bool,
    pub assets: usize,
    pub bytes: u64,
    pub phases: Vec<PhaseReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<PublishReceipt>,
}

impl PublishReport {
    pub fn is_complete(&self) -> bool {
        self.phases.iter().all(|phase| {
            phase.status == PhaseStatus::Complete || phase.status == PhaseStatus::Skipped
        })
    }

    pub fn failures(&self) -> Vec<&StepReport> {
        self.phases
            .iter()
            .flat_map(|phase| phase.steps.iter())
            .filter(|step| step.status == StepStatus::Fail)
            .collect()
    }

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

#[derive(Debug)]
pub struct ReportBuilder {
    report: PublishReport,
    open: Option<usize>,
}

impl ReportBuilder {
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

    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.report.dry_run = dry_run;
        self
    }

    pub fn plan(&mut self, assets: usize, bytes: u64) {
        self.report.assets = assets;
        self.report.bytes = bytes;
    }

    pub fn phase(&mut self, name: impl Into<String>) -> &mut Self {
        self.report.phases.push(PhaseReport {
            name: name.into(),
            status: PhaseStatus::Complete,
            steps: Vec::new(),
        });
        self.open = Some(self.report.phases.len() - 1);
        self
    }

    pub fn skip(&mut self) {
        if let Some(index) = self.open
            && let Some(phase) = self.report.phases.get_mut(index)
        {
            phase.status = PhaseStatus::Skipped;
        }
    }

    pub fn fail(&mut self) {
        if let Some(index) = self.open
            && let Some(phase) = self.report.phases.get_mut(index)
        {
            phase.status = PhaseStatus::Failed;
        }
    }

    pub fn ok(&mut self, label: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Ok, None, None)
    }

    pub fn info(&mut self, label: impl Into<String>, detail: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Info, Some(detail.into()), None)
    }

    pub fn sized(&mut self, label: impl Into<String>, bytes: u64) -> &mut Self {
        self.step(label, StepStatus::Ok, None, Some(bytes))
    }

    pub fn warn(&mut self, label: impl Into<String>, detail: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Warn, Some(detail.into()), None)
    }

    pub fn failed(&mut self, label: impl Into<String>, detail: impl Into<String>) -> &mut Self {
        self.step(label, StepStatus::Fail, Some(detail.into()), None)
    }

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

    pub fn notice(&mut self, notice: Notice) -> &mut Self {
        self.report.notices.push(notice);
        self
    }

    pub fn receipt(&mut self, receipt: PublishReceipt) -> &mut Self {
        self.report.receipt = Some(receipt);
        self
    }

    pub fn phases(&self) -> &[PhaseReport] {
        &self.report.phases
    }

    pub fn finish(self) -> PublishReport {
        self.report
    }
}
