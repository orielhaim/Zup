//! What the bootstrapper says while it works.
//!
//! A windowed bootstrapper has no console, so these lines are not for a person
//! looking at it — they are for a log, and for the headless case where the
//! bootstrapper's own stdout is the machine-readable stream. Both forms speak the
//! acquisition event set unchanged, so the events a consumer sees before the
//! handoff are the same events, with the same names, as the ones it sees after
//! it.
//!
//! One emitter owns one thread, so a slow terminal cannot stall a transfer and a
//! long download still reports while it runs. Joining it is what guarantees the
//! terminal event was printed.

use zup_acquire::{AcquisitionEvent, format_bytes};

/// Where the bootstrapper's own output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// One moving line per sample, for a terminal.
    #[default]
    Human,
    /// One JSON object per line, for a consumer that reads stdout.
    Jsonl,
}

/// Report acquisition events on a background thread.
pub struct Emitter {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Emitter {
    /// Consume `events` on a background thread.
    pub fn spawn(events: tokio::sync::mpsc::Receiver<AcquisitionEvent>, format: Format) -> Self {
        let mut events = events;
        Self {
            handle: Some(std::thread::spawn(move || match format {
                Format::Human => {
                    let mut line = Human::default();
                    while let Some(event) = events.blocking_recv() {
                        line.report(&event);
                    }
                }
                Format::Jsonl => {
                    while let Some(event) = events.blocking_recv() {
                        report_json(&event);
                    }
                }
            })),
        }
    }

    /// Wait for the emitter to finish, which happens when the sink is dropped.
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Emitter {
    fn drop(&mut self) {
        // A detached emitter would still be printing when the process exits,
        // which is how a terminal event goes missing from a log.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A terminal reporter: one moving line for progress, one line per event
/// otherwise.
#[derive(Debug, Default)]
pub struct Human {
    moving: bool,
}

impl Human {
    /// Report one event, returning the line that was rendered.
    pub fn report(&mut self, event: &AcquisitionEvent) -> String {
        use std::io::Write;
        let line = render(event);
        let is_progress = matches!(event, AcquisitionEvent::DownloadProgress { .. });
        let mut out = std::io::stdout().lock();
        if is_progress {
            if self.moving {
                let _ = write!(out, "\r{line}");
            } else {
                let _ = writeln!(out, "{line}");
                self.moving = true;
            }
        } else {
            if self.moving {
                let _ = writeln!(out);
                self.moving = false;
            }
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
        line
    }
}

fn report_json(event: &AcquisitionEvent) {
    use std::io::Write;
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// One line for one event.
///
/// Aggregate, never per-chunk: a bootstrapper that narrated every read would be
/// unreadable in a log and useless in a window.
pub fn render(event: &AcquisitionEvent) -> String {
    match event {
        AcquisitionEvent::ReleaseResolved {
            app_id,
            channel,
            version,
            release_digest,
        } => format!("{app_id} {version} ({channel}) release {release_digest}"),
        AcquisitionEvent::VariantSelected {
            variant,
            target,
            compatibility,
        } => format!("selected {variant} for {target} ({compatibility})"),
        AcquisitionEvent::AcquisitionStarted {
            items, estimate, ..
        } => {
            format!("acquiring {items} objects · {estimate}")
        }
        AcquisitionEvent::DownloadProgress { progress } => {
            let percent = progress
                .percent()
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "starting".to_owned());
            format!("{percent} {}", progress.detail())
        }
        AcquisitionEvent::CacheHit { wire_bytes, .. } => {
            format!("cached {}", format_bytes(*wire_bytes))
        }
        AcquisitionEvent::Retrying {
            attempt,
            delay_ms,
            reason,
            ..
        } => format!("retry {attempt} in {delay_ms}ms: {reason}"),
        AcquisitionEvent::AcquisitionComplete {
            items,
            bytes,
            elapsed_ms,
        } => format!(
            "acquired {items} objects · {} in {elapsed_ms}ms",
            format_bytes(*bytes)
        ),
        AcquisitionEvent::StagingComplete { staged, bytes } => {
            format!("staged {staged} objects · {}", format_bytes(*bytes))
        }
        AcquisitionEvent::Cancelled { .. } => "cancelled".to_owned(),
        AcquisitionEvent::Failed {
            message, reasons, ..
        } => {
            if reasons.is_empty() {
                format!("failed: {message}")
            } else {
                format!("failed: {message} · {}", reasons.join(" · "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_acquire::{AcquisitionEstimate, AcquisitionPhase, AcquisitionProgress, ContentReason};

    fn progress(completed: u64, total: u64) -> AcquisitionProgress {
        let mut progress = AcquisitionProgress {
            phase: AcquisitionPhase::Downloading,
            completed_bytes: completed,
            total_bytes: total,
            cached_bytes: 0,
            cached_items: 0,
            active_transfers: 2,
            completed_items: 1,
            total_items: 4,
            elapsed: std::time::Duration::from_secs(2),
            throughput: 0,
            eta: None,
            retries: 0,
        };
        progress.recompute();
        progress
    }

    #[test]
    fn progress_renders_as_a_percentage_and_a_line() {
        let line = render(&AcquisitionEvent::DownloadProgress {
            progress: Box::new(progress(63, 100)),
        });
        assert!(line.starts_with("63%"), "{line}");
        assert!(line.contains("63 B of 100 B"), "{line}");
    }

    #[test]
    fn a_terminal_emitter_keeps_one_moving_progress_line() {
        let (sink, receiver) = zup_acquire::ProgressSink::channel(8);
        sink.offer(AcquisitionEvent::DownloadProgress {
            progress: Box::new(progress(1, 2)),
        });
        sink.offer(AcquisitionEvent::AcquisitionComplete {
            items: 2,
            bytes: 2,
            elapsed_ms: 2,
        });
        let mut human = Human::default();
        human.report(&AcquisitionEvent::DownloadProgress {
            progress: Box::new(progress(1, 2)),
        });
        assert!(human.moving);
        human.report(&AcquisitionEvent::AcquisitionComplete {
            items: 2,
            bytes: 2,
            elapsed_ms: 2,
        });
        assert!(!human.moving, "a terminal event ends the moving line");
        drop(receiver);
    }

    #[test]
    fn a_failure_reports_one_line_per_source() {
        let line = render(&AcquisitionEvent::Failed {
            kind: "unavailable",
            digest: None,
            message: "no source could acquire payload blob".to_owned(),
            reasons: vec![
                "the CDN reset twice".to_owned(),
                "the mirror does not carry it".to_owned(),
            ],
            machine_unchanged: true,
        });
        assert!(line.contains("the CDN reset twice"), "{line}");
        assert!(line.contains("the mirror does not carry it"), "{line}");
    }

    #[test]
    fn the_estimate_is_rendered_unchanged() {
        let line = render(&AcquisitionEvent::AcquisitionStarted {
            variant: "x64".to_owned(),
            items: 3,
            estimate: AcquisitionEstimate {
                download_bytes: 1024 * 1024,
                install_bytes: 4 * 1024 * 1024,
                cached_bytes: 0,
                cached_items: 0,
                missing_items: 3,
            },
        });
        assert!(line.contains("1.00 MiB"), "{line}");
        assert!(line.contains("4.00 MiB"), "{line}");
    }

    #[test]
    fn a_cache_hit_is_a_line_and_not_a_download() {
        assert_eq!(
            render(&AcquisitionEvent::CacheHit {
                kind: "payload blob",
                digest: "ab".to_owned(),
                wire_bytes: 2048,
            }),
            "cached 2.00 KiB"
        );
        assert_eq!(ContentReason::Runtime.group(), "runtime");
    }
}
