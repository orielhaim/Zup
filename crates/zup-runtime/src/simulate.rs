use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::RuntimeEvent;
use crate::session::{CancellationHandle, InstallOutcome, emit_terminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulatedLifecycle {
    Install,
    Upgrade,
    Modify,
    Repair,
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulatedJob {
    pub lifecycle: SimulatedLifecycle,
    pub download_bytes: u64,
    pub file_bytes: u64,
    pub system_changes: bool,
}

pub fn run_simulated(
    job: SimulatedJob,
    cancel: CancellationHandle,
    events: broadcast::Sender<RuntimeEvent>,
) -> InstallOutcome {
    let mut simulation = Simulation::new(job, Instant::now());
    loop {
        match simulation.poll(Instant::now(), cancel.is_cancelled()) {
            Step::Wait => thread::sleep(Duration::from_millis(40)),
            Step::Event(event) => {
                if events.send(event).is_err() {
                    return InstallOutcome::Cancelled;
                }
            }
            Step::Finished(outcome) => {
                emit_terminal(&events, &Ok(outcome.clone()));
                return outcome;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Stage {
    label: &'static str,
    weight: u64,
    duration: Duration,
}

struct Simulation {
    stages: Vec<Stage>,
    index: usize,
    stage_started: Instant,
    finished_weight: u64,
    total_weight: u64,
    cancelled_at: Option<Instant>,
    safe_wait: Duration,
    shown: Option<(u32, usize)>,
}

enum Step {
    Wait,
    Event(RuntimeEvent),
    Finished(InstallOutcome),
}

impl Simulation {
    fn new(job: SimulatedJob, now: Instant) -> Self {
        let stages = stages(&job);
        let total_weight = stages.iter().map(|stage| stage.weight).sum();
        let safe_wait = Duration::from_millis(575);
        Self {
            stages,
            index: 0,
            stage_started: now,
            finished_weight: 0,
            total_weight,
            cancelled_at: None,
            safe_wait,
            shown: None,
        }
    }

    fn poll(&mut self, now: Instant, cancelled: bool) -> Step {
        if cancelled && self.cancelled_at.is_none() {
            self.cancelled_at = Some(now);
        }
        if let Some(at) = self.cancelled_at
            && now.saturating_duration_since(at) >= self.safe_wait
        {
            return Step::Finished(InstallOutcome::Cancelled);
        }
        while self.index < self.stages.len() {
            let duration = self.stages[self.index].duration;
            let elapsed = now.saturating_duration_since(self.stage_started);
            if elapsed < duration {
                return self.event(now);
            }
            self.finished_weight += self.stages[self.index].weight;
            self.index += 1;
            self.stage_started += duration;
        }
        Step::Finished(InstallOutcome::Committed)
    }

    fn event(&mut self, now: Instant) -> Step {
        let stage = self.stages[self.index];
        let elapsed = now.saturating_duration_since(self.stage_started);
        let fraction = elapsed.as_millis().saturating_mul(stage.weight as u128)
            / stage.duration.as_millis().max(1);
        let completed = self
            .finished_weight
            .saturating_add(fraction as u64)
            .min(self.total_weight);
        let percent = ((completed as u128) * 100 / (self.total_weight.max(1) as u128)) as u32;
        if self.shown == Some((percent, self.index)) {
            return Step::Wait;
        }
        self.shown = Some((percent, self.index));
        Step::Event(RuntimeEvent::Progress {
            completed,
            total: self.total_weight,
            action: stage.label.to_owned(),
        })
    }
}

fn stages(job: &SimulatedJob) -> Vec<Stage> {
    let mut stages = vec![stage("Preparing…", 4, 450, 1200)];
    match job.lifecycle {
        SimulatedLifecycle::Uninstall => {
            let files = file_span(job.file_bytes);
            stages.push(stage("Removing files…", 48, files.0, files.1));
            if job.system_changes {
                stages.push(stage("Removing system changes…", 14, 500, 1500));
            }
        }
        SimulatedLifecycle::Repair => {
            stages.push(stage("Verifying installed files…", 12, 500, 1400));
            let files = file_span(job.file_bytes);
            stages.push(stage("Installing files…", 40, files.0, files.1));
            if job.system_changes {
                stages.push(stage("Registering system changes…", 12, 450, 1300));
            }
        }
        SimulatedLifecycle::Install | SimulatedLifecycle::Upgrade | SimulatedLifecycle::Modify => {
            if job.download_bytes > 0 {
                stages.push(stage("Downloading required components…", 22, 1100, 3200));
            }
            stages.push(stage("Verifying the package…", 8, 350, 1100));
            let files = file_span(job.file_bytes);
            stages.push(stage("Installing files…", 46, files.0, files.1));
            if job.system_changes {
                stages.push(stage("Registering system changes…", 14, 500, 1600));
            }
        }
    }
    stages.push(stage("Finishing…", 5, 300, 800));
    stages
}

fn file_span(bytes: u64) -> (u64, u64) {
    let scale = (bytes / (24 * 1024 * 1024)).clamp(1, 3);
    (700 * scale, 1600 * scale)
}

fn stage(label: &'static str, weight: u64, min_ms: u64, max_ms: u64) -> Stage {
    Stage {
        label,
        weight,
        duration: Duration::from_millis((min_ms + max_ms) / 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install() -> SimulatedJob {
        SimulatedJob {
            lifecycle: SimulatedLifecycle::Install,
            download_bytes: 0,
            file_bytes: 80 * 1024 * 1024,
            system_changes: true,
        }
    }

    #[test]
    fn stages_are_reported_in_order_before_the_operation_commits() {
        let start = Instant::now();
        let mut simulation = Simulation::new(install(), start);
        let mut labels = Vec::new();
        let mut now = start;
        loop {
            now += Duration::from_millis(250);
            match simulation.poll(now, false) {
                Step::Event(RuntimeEvent::Progress { action, .. }) => {
                    if labels.last() != Some(&action) {
                        labels.push(action);
                    }
                }
                Step::Finished(outcome) => {
                    assert_eq!(outcome, InstallOutcome::Committed);
                    break;
                }
                Step::Wait | Step::Event(_) => {}
            }
            assert!(
                now.saturating_duration_since(start) < Duration::from_secs(30),
                "the simulation did not finish"
            );
        }
        assert!(
            labels.iter().any(|label| label.contains("Preparing")),
            "prepare is a stage: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("Verifying")),
            "verify is a stage: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Installing files")),
            "files are a stage: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("system")),
            "system changes are a stage: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("Finishing")),
            "finish is a stage: {labels:?}"
        );
    }

    #[test]
    fn stop_ends_the_operation_at_a_safe_point_without_committing() {
        let start = Instant::now();
        let mut simulation = Simulation::new(install(), start);
        let mut now = start;
        let outcome = loop {
            now += Duration::from_millis(100);
            match simulation.poll(now, true) {
                Step::Finished(outcome) => break outcome,
                Step::Wait | Step::Event(_) => {}
            }
            assert!(
                now.saturating_duration_since(start) < Duration::from_secs(3),
                "stop waits for a safe point, not for the rest of the operation"
            );
        };
        assert_eq!(outcome, InstallOutcome::Cancelled);
    }
}
