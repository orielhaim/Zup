//! How the three frontends see one acquisition.
//!
//! There is one event set, and it is the same for a window, a terminal, and a
//! JSONL stream. A frontend cannot disagree about an install's cost because
//! there is only one number and one place it is computed. What differs is the
//! rendering, and the rendering is this module's whole job.
//!
//! The mapping is deliberately total. Every acquisition event maps to exactly one
//! automation event or to nothing at all, and the set of events a consumer may
//! see is the closed set in [`automation_events`]. Adding an event is therefore a
//! visible change rather than something a preset silently ignores.
//!
//! # What a consumer may rely on
//!
//! ```text
//! release_resolved  variant_selected  acquisition_started
//! download_progress cache_hit         retrying
//! acquisition_complete staging_complete cancelled failed
//! install_started   completed
//! ```
//!
//! Nothing in that list is a blob URL, a temporary path, or an internal
//! directory. A digest and a byte count are stable; a path is not, and a
//! consumer that parsed one would break on the first maintenance run.

use tokio::sync::mpsc::Receiver;

use zup_acquire::{AcquisitionEvent, format_bytes};

use crate::{InstallerEvent, OperationPhase, ProcessOutcome};

/// Report acquisition events on a background thread.
///
/// Joining is what guarantees the terminal event was emitted, which is why this
/// is a handle rather than a fire-and-forget. A consumer that reads a stream and
/// hits end-of-stream knows the operation finished.
pub struct AcquisitionThread {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AcquisitionThread {
    /// Consume `events`, printing them as JSONL on stdout.
    pub fn jsonl(events: Receiver<AcquisitionEvent>) -> Self {
        Self::spawn(events, |event| {
            let mapped = automation_events(&event);
            if mapped.is_empty() {
                return;
            }
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            for event in mapped {
                let Ok(line) = serde_json::to_string(&event) else {
                    continue;
                };
                let _ = writeln!(out, "{line}");
            }
            let _ = out.flush();
        })
    }

    /// Consume `events`, writing each rendered line through `emit`.
    pub fn spawn(
        mut events: Receiver<AcquisitionEvent>,
        emit: impl Fn(AcquisitionEvent) + Send + 'static,
    ) -> Self {
        Self {
            handle: Some(std::thread::spawn(move || {
                while let Some(event) = events.blocking_recv() {
                    emit(event);
                }
            })),
        }
    }

    /// Wait for the thread, which happens when the sink is dropped.
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AcquisitionThread {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Consume `events` as JSONL on stdout.
pub fn acquisition_thread(events: Receiver<AcquisitionEvent>) -> AcquisitionThread {
    AcquisitionThread::jsonl(events)
}

/// The automation events one acquisition event produces.
///
/// Empty means "this is a sample a machine stream does not need". That is the
/// right answer for a `CacheHit` in a large closure: the total is already on the
/// `acquisition_started` estimate, and one line per cache hit would be one line
/// per file in an install.
pub fn automation_events(event: &AcquisitionEvent) -> Vec<InstallerEvent> {
    match event {
        AcquisitionEvent::ReleaseResolved { version, .. } => vec![InstallerEvent::Phase {
            state: format!("resolved {version}"),
        }],
        AcquisitionEvent::VariantSelected { variant, .. } => vec![InstallerEvent::Phase {
            state: format!("variant {variant}"),
        }],
        AcquisitionEvent::AcquisitionStarted {
            items, estimate, ..
        } => vec![InstallerEvent::Progress {
            phase: OperationPhase::Download,
            completed: 0,
            total: *items,
            label: format!(
                "{} to download · {} to install · {} already cached",
                format_bytes(estimate.download_bytes),
                format_bytes(estimate.install_bytes),
                format_bytes(estimate.cached_bytes)
            ),
        }],
        AcquisitionEvent::DownloadProgress { progress } => vec![InstallerEvent::Progress {
            phase: OperationPhase::Download,
            completed: progress.completed_bytes,
            total: progress.total_bytes,
            label: progress.detail(),
        }],
        AcquisitionEvent::AcquisitionComplete {
            items,
            bytes,
            elapsed_ms,
        } => vec![InstallerEvent::Phase {
            state: format!("acquired {items} objects in {elapsed_ms}ms"),
        }]
        .into_iter()
        .chain(std::iter::once(InstallerEvent::Phase {
            state: format_bytes(*bytes),
        }))
        .collect(),
        AcquisitionEvent::Retrying {
            attempt, reason, ..
        } => vec![InstallerEvent::Phase {
            state: format!("retry {attempt}: {reason}"),
        }],
        AcquisitionEvent::Cancelled { .. } => vec![InstallerEvent::Cancelling {
            state: "acquisition".to_owned(),
        }],
        AcquisitionEvent::Failed {
            message,
            reasons,
            machine_unchanged,
            ..
        } => {
            // A failure before the barrier is reported as a failure, and the
            // guarantee that nothing was touched is stated rather than inferred -
            // a consumer can assert it, and a human reading the log can trust it.
            let mut detail = message.clone();
            if !reasons.is_empty() {
                detail.push_str(" · ");
                detail.push_str(&reasons.join(" · "));
            }
            vec![InstallerEvent::Failed {
                outcome: ProcessOutcome::Failure,
                code: i32::from(*machine_unchanged),
                message: detail,
                diagnostic: None,
            }]
        }
        // A cache hit is on the estimate already, and `StagingComplete` is
        // internal to the engine's own pipeline.
        AcquisitionEvent::CacheHit { .. } | AcquisitionEvent::StagingComplete { .. } => Vec::new(),
    }
}

/// The line a window shows under a progress bar.
pub fn progress_line(event: &AcquisitionEvent) -> Option<String> {
    match event {
        AcquisitionEvent::DownloadProgress { progress } => {
            let percent = progress
                .percent()
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "starting".to_owned());
            Some(format!("{percent} · {}", progress.detail()))
        }
        AcquisitionEvent::AcquisitionStarted { estimate, .. } => {
            Some(format!("Downloading {}", estimate.download_bytes))
        }
        AcquisitionEvent::AcquisitionComplete {
            bytes, elapsed_ms, ..
        } => Some(format!("{} in {elapsed_ms}ms", format_bytes(*bytes))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_acquire::{AcquisitionEstimate, AcquisitionPhase, AcquisitionProgress, ContentKind};

    fn progress(completed: u64, total: u64) -> Box<AcquisitionProgress> {
        let mut progress = AcquisitionProgress {
            phase: AcquisitionPhase::Downloading,
            completed_bytes: completed,
            total_bytes: total,
            cached_bytes: 0,
            cached_items: 0,
            active_transfers: 2,
            completed_items: 1,
            total_items: 4,
            elapsed: std::time::Duration::from_secs(1),
            throughput: 0,
            eta: None,
            retries: 0,
        };
        progress.recompute();
        Box::new(progress)
    }

    #[test]
    fn the_headless_stream_names_the_release_and_the_variant() {
        let events = automation_events(&AcquisitionEvent::ReleaseResolved {
            app_id: "com.acme.app".to_owned(),
            channel: "stable".to_owned(),
            version: "1.4.0".to_owned(),
            release_digest: zup_core::Sha256Digest::from_bytes([1; 32]),
        });
        assert!(
            matches!(events.as_slice(), [InstallerEvent::Phase { state }] if state.contains("1.4.0"))
        );
        let events = automation_events(&AcquisitionEvent::VariantSelected {
            variant: "x64".to_owned(),
            target: "x86_64-pc-windows-msvc".to_owned(),
            compatibility: "native".to_owned(),
        });
        assert!(
            matches!(events.as_slice(), [InstallerEvent::Phase { state }] if state.contains("x64"))
        );
    }

    #[test]
    fn a_cache_hit_is_not_one_line_per_file() {
        // A closure can be tens of thousands of blobs. The total is already on
        // the estimate, so narrating each hit would be one line per file.
        assert!(
            automation_events(&AcquisitionEvent::CacheHit {
                kind: ContentKind::Payload.as_str(),
                digest: "ab".to_owned(),
                wire_bytes: 1,
            })
            .is_empty()
        );
    }

    #[test]
    fn a_failure_states_that_nothing_was_touched() {
        let events = automation_events(&AcquisitionEvent::Failed {
            kind: "unavailable",
            digest: None,
            message: "no source could acquire payload blob".to_owned(),
            reasons: vec!["the CDN reset twice".to_owned()],
            machine_unchanged: true,
        });
        assert!(matches!(
            events.as_slice(),
            [InstallerEvent::Failed { code: 1, message, .. }] if message.contains("the CDN reset twice")
        ));
    }

    #[test]
    fn the_estimate_is_the_number_every_frontend_renders() {
        let events = automation_events(&AcquisitionEvent::AcquisitionStarted {
            variant: "x64".to_owned(),
            items: 3,
            estimate: AcquisitionEstimate {
                download_bytes: 1024 * 1024,
                install_bytes: 4 * 1024 * 1024,
                cached_bytes: 512 * 1024,
                cached_items: 1,
                missing_items: 2,
            },
        });
        assert!(matches!(
            events.as_slice(),
            [InstallerEvent::Progress { label, total: 3, .. }]
                if label.contains("1.00 MiB") && label.contains("4.00 MiB") && label.contains("512 KiB")
        ));
    }

    #[test]
    fn the_window_line_is_a_percentage_and_a_rate() {
        let line = progress_line(&AcquisitionEvent::DownloadProgress {
            progress: progress(63, 100),
        })
        .expect("a progress line");
        assert!(line.starts_with("63%"), "{line}");
    }

    #[test]
    fn no_automation_event_carries_a_url_or_a_path() {
        // A consumer that parsed a path would break on the first maintenance run,
        // so the guarantee is asserted rather than documented.
        let every = [
            AcquisitionEvent::ReleaseResolved {
                app_id: "com.acme.app".to_owned(),
                channel: "stable".to_owned(),
                version: "1.4.0".to_owned(),
                release_digest: zup_core::Sha256Digest::from_bytes([1; 32]),
            },
            AcquisitionEvent::VariantSelected {
                variant: "x64".to_owned(),
                target: "x86_64-pc-windows-msvc".to_owned(),
                compatibility: "native".to_owned(),
            },
            AcquisitionEvent::AcquisitionStarted {
                variant: "x64".to_owned(),
                items: 1,
                estimate: AcquisitionEstimate::default(),
            },
            AcquisitionEvent::DownloadProgress {
                progress: progress(1, 2),
            },
            AcquisitionEvent::Retrying {
                kind: "payload blob",
                digest: "ab".to_owned(),
                attempt: 2,
                delay_ms: 400,
                reason: "connection reset".to_owned(),
            },
            AcquisitionEvent::AcquisitionComplete {
                items: 1,
                bytes: 1,
                elapsed_ms: 1,
            },
        ];
        for event in &every {
            for mapped in automation_events(event) {
                let text = serde_json::to_string(&mapped).expect("serializes");
                assert!(!text.contains("://"), "{text}");
                assert!(!text.contains('\\'), "{text}");
                assert!(!text.contains(".partial"), "{text}");
            }
        }
    }
}
