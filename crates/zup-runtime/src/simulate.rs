//! A lifecycle that takes time and changes nothing.
//!
//! The same stages a real operation walks - prepare, download, verify, files,
//! system, finish - each lasting a different amount of time, reporting through
//! the same [`RuntimeEvent`]s. Cancellation stops at the next safe point rather
//! than at the end of the current stage. Nothing here writes a file, a
//! registry value, or a shortcut: the clock is the work.

use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use crate::events::RuntimeEvent;
use crate::session::{CancellationHandle, InstallOutcome, emit_terminal};

/// Which lifecycle is being simulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulatedLifecycle {
    Install,
    Upgrade,
    Modify,
    Repair,
    Uninstall,
}

/// What the simulated lifecycle has to get through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulatedJob {
    pub lifecycle: SimulatedLifecycle,
    /// Bytes that have to be fetched before the files can be written.
    pub download_bytes: u64,
    /// Bytes the operation writes or removes.
    pub file_bytes: u64,
    /// Shortcuts, services, PATH, protocols, or associations.
    pub system_changes: bool,
}

/// Run `job` to completion, or until `cancel` is signalled.
///
/// Events are the ones a real operation emits, including the terminal event.
/// A dropped receiver ends the run as cancelled: nobody is left to watch it.
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
        let mut rng = Rng::from_time();
        let stages = stages(&job, &mut rng);
        let total_weight = stages.iter().map(|stage| stage.weight).sum();
        let safe_wait = Duration::from_millis(rng.gen_range(350, 800));
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

fn stages(job: &SimulatedJob, rng: &mut Rng) -> Vec<Stage> {
    let mut stages = vec![stage(rng, "Preparing…", 4, 450, 1200)];
    match job.lifecycle {
        SimulatedLifecycle::Uninstall => {
            let files = file_span(job.file_bytes);
            stages.push(stage(rng, "Removing files…", 48, files.0, files.1));
            if job.system_changes {
                stages.push(stage(rng, "Removing system changes…", 14, 500, 1500));
            }
        }
        SimulatedLifecycle::Repair => {
            stages.push(stage(rng, "Verifying installed files…", 12, 500, 1400));
            let files = file_span(job.file_bytes);
            stages.push(stage(rng, "Installing files…", 40, files.0, files.1));
            if job.system_changes {
                stages.push(stage(rng, "Registering system changes…", 12, 450, 1300));
            }
        }
        SimulatedLifecycle::Install | SimulatedLifecycle::Upgrade | SimulatedLifecycle::Modify => {
            if job.download_bytes > 0 {
                stages.push(stage(
                    rng,
                    "Downloading required components…",
                    22,
                    1100,
                    3200,
                ));
            }
            stages.push(stage(rng, "Verifying the package…", 8, 350, 1100));
            let files = file_span(job.file_bytes);
            stages.push(stage(rng, "Installing files…", 46, files.0, files.1));
            if job.system_changes {
                stages.push(stage(rng, "Registering system changes…", 14, 500, 1600));
            }
        }
    }
    stages.push(stage(rng, "Finishing…", 5, 300, 800));
    stages
}

/// How long the file stage lasts, growing with the amount of data and still random.
fn file_span(bytes: u64) -> (u64, u64) {
    let scale = (bytes / (24 * 1024 * 1024)).clamp(1, 3);
    (700 * scale, 1600 * scale)
}

fn stage(rng: &mut Rng, label: &'static str, weight: u64, min_ms: u64, max_ms: u64) -> Stage {
    Stage {
        label,
        weight,
        duration: Duration::from_millis(rng.gen_range(min_ms, max_ms)),
    }
}

struct Rng(u64);

impl Rng {
    fn from_time() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self(nanos | 1)
    }

    fn gen_range(&mut self, min: u64, max: u64) -> u64 {
        if max <= min {
            return min;
        }
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        min + x % (max - min + 1)
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
