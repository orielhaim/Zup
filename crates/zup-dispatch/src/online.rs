//! The thin path: resolve a release, verify a runtime, start it.
//!
//! This is a bootstrapper, and the whole of what it is allowed to do is here:
//!
//! ```text
//! load its embedded trust block and durable TUF state
//! resolve an authenticated release
//! select the one variant this host can run
//! acquire the native runtime into a verified cache
//! verify it against the digest the release named
//! write a handoff, start the runtime, forward its outcome
//! ```
//!
//! It installs nothing. It touches no registry, creates no service, elevates
//! nothing, and runs no prerequisite. Every one of those is inside the selected
//! native runtime, under the transaction engine, in its own architecture - which
//! is why this program can be small enough to run under an emulation layer on a
//! machine whose native variant is something else.
//!
//! # What it is not allowed to believe
//!
//! The bootstrapper is the *untrusted* half of the pair, because it is the half
//! that talked to a network. It verifies everything it hands over, and the
//! runtime verifies all of it again. The handoff it writes carries only digests,
//! names, and counts; the runtime checks `handoff.runtime` against the hash of
//! its own file, which is what makes the rest of the document meaningful.

use std::path::{Path, PathBuf};

use zup_acquire::{
    AcquisitionSession, CachePolicy, HandoffMode, HostProfile, OnlineTrust, ProgressSink,
    RuntimeHandoff, SessionSummary,
};
use zup_artifact::ArtifactIndex;
use zup_update::{ReleaseResolver, TrustContext, bootstrap_scheduler};
use zup_windows::{HandOff, write_handoff};

use crate::Outcome;

use crate::events;

/// What the bootstrapper needs from whoever is running it.
///
/// These are the only two inputs, and both are *configuration* rather than
/// authority: where the machine keeps its state, and an optional local tree to
/// satisfy the closure from. A hostile value for either produces content that
/// fails its digest check, never content that passes one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootstrapRequest {
    /// A local web tree to read first, for a USB stick or an enterprise share.
    pub source: Option<PathBuf>,
    /// Where the machine keeps TUF state and the content cache.
    pub state_root: Option<PathBuf>,
    /// Report acquisition events on stdout as JSONL.
    pub machine_readable: bool,
}

/// A local source name that appears in diagnostics.
const SEED_NAME: &str = "local source";

/// Run the thin path for the artifact described by `index`.
///
/// `scope` is the installation scope the artifact's trust block declares, which
/// the bootstrapper knows from the artifact rather than from a command line
/// argument - a user double-clicking the artifact never chose one. It is the same
/// scope the launcher passes to the runtime it starts, and the same scope that
/// decides which state root this process reads, so a thin install lands where the
/// project said it would.
pub fn run(
    index: &ArtifactIndex,
    request: &BootstrapRequest,
    scope: zup_core::SelectedScope,
    subsystem_gui: bool,
) -> Outcome {
    run_with(
        &zup_windows::WindowsLauncher,
        index,
        request,
        scope,
        subsystem_gui,
    )
}

/// Run the thin path, starting the runtime through `launcher`.
///
/// The seam exists so the whole path - resolve, select, acquire, verify, stage,
/// hand over - can be exercised without a child process, which is the only way to
/// assert on the thing that matters most about the handoff: exactly which
/// executable is started, with exactly which arguments, and with which
/// inheritance. Those are properties of a value, and a value has to be
/// inspectable to be testable.
pub fn run_with(
    launcher: &dyn zup_windows::Launcher,
    index: &ArtifactIndex,
    request: &BootstrapRequest,
    scope: zup_core::SelectedScope,
    subsystem_gui: bool,
) -> Outcome {
    let Some(trust) = index.artifact.trust.clone() else {
        return Outcome::Refused {
            detail: "this installer carries no trust block".to_owned(),
        };
    };
    if let Err(detail) = index.artifact.validate() {
        return Outcome::Refused {
            detail: detail.to_owned(),
        };
    }
    let state_root = match crate::state_root(request.state_root.as_deref(), scope) {
        Ok(root) => root,
        Err(detail) => return Outcome::Refused { detail },
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Outcome::Refused {
                detail: format!("no async runtime is available: {error}"),
            };
        }
    };

    let (sink, receiver) = ProgressSink::channel(64);
    // The emitter runs for as long as the sink does, so the terminal event is
    // printed before the process reports an outcome. Joining is what makes that
    // true; a detached thread would race the exit.
    let emitter = events::Emitter::spawn(
        receiver,
        if request.machine_readable {
            events::Format::Jsonl
        } else {
            events::Format::Human
        },
    );

    let outcome = runtime.block_on(acquire_and_start(ThinPath {
        launcher,
        trust: trust.clone(),
        request,
        state_root,
        scope,
        subsystem_gui,
        sink: sink.clone(),
    }));
    drop(sink);
    emitter.join();
    outcome
}

/// Everything the thin path was told, in one value.
///
/// These are the bootstrapper's inputs, and they are inputs rather than a
/// conversation: nothing in the path changes them after they are read, and
/// passing them as one value is what stops a positional list from growing a
/// silent reordering.
struct ThinPath<'a> {
    launcher: &'a dyn zup_windows::Launcher,
    trust: OnlineTrust,
    request: &'a BootstrapRequest,
    state_root: PathBuf,
    scope: zup_core::SelectedScope,
    /// Whether this build is a windowed one, which decides the handoff: a GUI
    /// bootstrapper has no console to pass on, so the runtime opens its own.
    subsystem_gui: bool,
    sink: ProgressSink,
}

async fn acquire_and_start(path: ThinPath<'_>) -> Outcome {
    let ThinPath {
        launcher,
        trust,
        request,
        state_root,
        scope,
        subsystem_gui,
        sink,
    } = path;
    let context = TrustContext {
        trust: trust.clone(),
        state_root,
        pin: trust.pin.clone(),
    };
    let mut resolver =
        match ReleaseResolver::new(context, HostProfile::native(), CachePolicy::Auto, sink) {
            Ok(resolver) => resolver,
            Err(error) => {
                return Outcome::Refused {
                    detail: error.to_string(),
                };
            }
        };
    if let Some(source) = &request.source {
        resolver = resolver.with_seed(SEED_NAME, source.clone());
    }

    let resolved = match resolver.resolve().await {
        Ok(resolved) => resolved,
        // A graph that authenticated and names nothing for this machine is not a
        // trust failure and not a download failure. It is the answer "this
        // computer cannot run this release", and a user needs to be told that
        // rather than "the update server could not be reached".
        Err(error @ zup_update::UpdateError::Graph(_))
            if error.to_string().contains("can run here") =>
        {
            return Outcome::Unsupported {
                detail: error.to_string(),
            };
        }
        Err(error) => {
            return Outcome::ResolveFailed {
                detail: error.to_string(),
            };
        }
    };
    let runtime_descriptor = match resolved.require_runtime() {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return Outcome::ResolveFailed {
                detail: error.to_string(),
            };
        }
    };

    // The bootstrapper fetches exactly one thing: the runtime it hands control
    // to. Everything else is the native runtime's problem, which is what keeps
    // this program from being an installer.
    let closure =
        match zup_acquire::AcquisitionPlan::build(vec![zup_acquire::AcquisitionItem::new(
            runtime_descriptor,
            zup_acquire::ContentReason::Runtime,
        )]) {
            Ok(plan) => plan,
            Err(error) => {
                return Outcome::VerificationFailed {
                    detail: error.to_string(),
                };
            }
        };
    let session = AcquisitionSession::new(
        closure,
        std::sync::Arc::clone(resolver.cache()),
        bootstrap_scheduler(),
    );
    let estimate = session.estimate();
    let outcome = match session
        .run(
            match resolver.chain() {
                Ok(chain) => chain,
                Err(error) => {
                    return Outcome::ResolveFailed {
                        detail: error.to_string(),
                    };
                }
            },
            std::sync::Arc::new(zup_acquire::NeverCancelled),
            resolver.progress(),
        )
        .await
    {
        Ok(barrier) => barrier.enter(),
        Err(error) => {
            let reasons = match &error {
                zup_acquire::AcquireError::Unavailable { reports, .. } => {
                    reports.iter().map(|report| report.to_string()).collect()
                }
                _ => Vec::new(),
            };
            return Outcome::AcquisitionFailed {
                detail: format!("{error}{}", suffix(&reasons)),
            };
        }
    };

    let Some(runtime) = outcome.get(&runtime_descriptor.digest) else {
        return Outcome::VerificationFailed {
            detail: "the acquired runtime is not in the verified cache".to_owned(),
        };
    };
    // A runtime is executable code, so it is re-hashed in full at the moment it
    // is about to be run, not merely length-checked.
    let verified = match runtime.read_to_end() {
        Ok(bytes) => bytes,
        Err(error) => {
            return Outcome::VerificationFailed {
                detail: error.to_string(),
            };
        }
    };

    let handoff_root = resolver.context().cache_dir().join("handoff");
    // The staged runtime is the maintenance runtime: the same native executable
    // an installation would persist, named for that role rather than for the
    // installer medium a person downloads.
    let runtime_path = handoff_root.join(zup_windows::MAINTENANCE_RUNTIME_DIRECTORY);
    if let Err(detail) = write_runtime(&runtime_path, &verified) {
        return Outcome::AcquisitionFailed { detail };
    }

    let summary = SessionSummary {
        total_bytes: estimate
            .download_bytes
            .saturating_add(estimate.cached_bytes),
        cached_bytes: estimate.cached_bytes,
        cache_hits: estimate.cached_items as u64,
        downloaded_items: estimate.missing_items as u64,
        elapsed_ms: outcome.elapsed.as_millis() as u64,
    };
    // The document the runtime re-reads is the one TUF delivered, and its byte
    // digest is the key the cache is addressed by. The release's own fingerprint
    // is a different number, because a release document names it as a field.
    let document_digest = resolved.descriptor_digest;
    let handoff = RuntimeHandoff {
        schema: zup_acquire::HANDOFF_SCHEMA,
        app_id: resolved.descriptor.app_id.clone(),
        release: resolved.descriptor.release_digest,
        document: document_digest,
        catalog: resolved.descriptor.catalog.digest,
        variant: resolved.variant.id.clone(),
        manifest: resolved.variant.manifest.digest,
        runtime: runtime_descriptor.digest,
        target: resolved.variant.target.clone(),
        frontend: resolved.variant.frontend.clone(),
        mode: HandoffMode::Install,
        scope: scope.to_string(),
        session: summary,
        components: Vec::new(),
    };
    let handoff_path = handoff_root.join(format!("{}.json", handoff.digest().to_hex()));
    let handoff_digest = match write_handoff(&handoff_path, &handoff) {
        Ok(digest) => digest,
        Err(error) => {
            return Outcome::Refused {
                detail: error.to_string(),
            };
        }
    };

    // The only things the runtime is told: where the cache is and which handoff to
    // read. Both are locations. Everything else it re-derives and re-verifies.
    let cache_root = resolver.context().cache_dir();
    let arguments = handoff_arguments(
        &cache_root,
        &handoff_path,
        Some(handoff_digest),
        request,
        scope,
        subsystem_gui,
    );
    let handoff = if subsystem_gui {
        HandOff::Silent
    } else {
        HandOff::Console
    };
    let child = match launcher.launch(&zup_windows::LaunchRequest {
        executable: runtime_path.clone(),
        arguments: arguments.clone(),
        handoff,
        working_directory: None,
    }) {
        Ok(child) => child,
        Err(error) => {
            return Outcome::LaunchFailed {
                detail: error.to_string(),
            };
        }
    };
    // The runtime owns installation correctness from here. Waiting is a
    // presentation choice, so a killed bootstrapper cannot corrupt it.
    let code = child.wait();
    classify(code)
}

fn handoff_arguments(
    cache_root: &Path,
    handoff_path: &Path,
    handoff_digest: Option<zup_core::Sha256Digest>,
    request: &BootstrapRequest,
    scope: zup_core::SelectedScope,
    subsystem_gui: bool,
) -> Vec<String> {
    let mut arguments = vec![
        "install".to_owned(),
        "--acquired".to_owned(),
        cache_root.display().to_string(),
        "--handoff".to_owned(),
        handoff_path.display().to_string(),
        "--scope".to_owned(),
        scope.to_string(),
    ];
    if let Some(digest) = handoff_digest {
        arguments.push("--handoff-digest".to_owned());
        arguments.push(digest.to_hex());
    }
    if subsystem_gui {
        arguments.push("--ui".to_owned());
        arguments.push("--non-interactive".to_owned());
    }
    if request.machine_readable {
        arguments.push("--output".to_owned());
        arguments.push("jsonl".to_owned());
    }
    if let Some(source) = &request.source {
        arguments.push("--source".to_owned());
        arguments.push(source.display().to_string());
    }
    arguments
}

/// Turn a native runtime's exit code into a typed outcome.
///
/// The codes are the ones `zup` already uses for machine output, so a script
/// that reads them needs no second table.
pub fn classify(code: i32) -> Outcome {
    match code {
        0 => Outcome::Completed { code: 0 },
        3010 => Outcome::RebootRequired { code },
        7 => Outcome::RecoveryRequired { code },
        _ => Outcome::InstallerFailed { code },
    }
}

/// Append one line per source that was tried, which is the difference between a
/// report a person can act on and one that says only that something failed.
fn suffix(reasons: &[String]) -> String {
    if reasons.is_empty() {
        String::new()
    } else {
        format!("\n  {}", reasons.join("\n  "))
    }
}

fn write_runtime(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    zup_windows::write_durable(path, bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_runtime_outcome_is_forwarded_rather_than_collapsed() {
        assert_eq!(classify(0), Outcome::Completed { code: 0 });
        assert_eq!(classify(3010), Outcome::RebootRequired { code: 3010 });
        assert_eq!(classify(7), Outcome::RecoveryRequired { code: 7 });
        assert_eq!(classify(1), Outcome::InstallerFailed { code: 1 });
        assert_eq!(
            Outcome::RebootRequired { code: 3010 }.describe(),
            "the installation needs a restart before it can finish (exit code 3010)"
        );
    }

    #[test]
    fn the_handoff_passes_locations_and_nothing_else() {
        let arguments = handoff_arguments(
            std::path::Path::new(r"C:\state\content"),
            std::path::Path::new(r"C:\state\content\handoff\abc.json"),
            Some(zup_core::Sha256Digest::from_bytes([1; 32])),
            &BootstrapRequest {
                source: Some(std::path::PathBuf::from(r"X:\Mirror")),
                state_root: None,
                machine_readable: true,
            },
            zup_core::SelectedScope::User,
            true,
        );
        // No payload root, no plan JSON, no URL: the runtime rebuilds all of it
        // from the authenticated descriptors and the cache it can see.
        assert!(!arguments.iter().any(|argument| argument.contains("://")));
        assert!(!arguments.iter().any(|argument| argument.contains(".zup")));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--acquired", r"C:\state\content"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--source", r"X:\Mirror"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--output", "jsonl"])
        );
        assert!(arguments.contains(&"--ui".to_owned()));
    }

    /// The scope a thin artifact installs into is the one its **trust block**
    /// declares, carried all the way to the `--scope` the runtime is launched
    /// with.
    ///
    /// A thin artifact carries no variant manifest, so the scope the
    /// application's plan declares has nowhere else to travel, and a launcher
    /// that defaulted it would install a `machine`-scoped application into the
    /// user's profile and report success. This is the test that says the constant
    /// is not there.
    #[test]
    fn a_thin_artifacts_scope_travels_to_the_runtime_it_starts() {
        for (trust, expected) in [
            (zup_acquire::ThinScope::User, "user"),
            (zup_acquire::ThinScope::Machine, "machine"),
        ] {
            let scope = trust.selected();
            let arguments = handoff_arguments(
                std::path::Path::new(r"C:\state\content"),
                std::path::Path::new(r"C:\state\content\handoff\abc.json"),
                Some(zup_core::Sha256Digest::from_bytes([1; 32])),
                &BootstrapRequest {
                    source: None,
                    state_root: None,
                    machine_readable: false,
                },
                scope,
                false,
            );
            assert!(
                arguments
                    .iter()
                    .position(|argument| argument == "--scope")
                    .map(|at| arguments[at + 1].as_str() == expected)
                    .unwrap_or(false),
                "a trust block declaring {trust:?} reached the runtime as {arguments:?}"
            );
        }
    }
}
