//! Acquires a verified runtime. The runtime independently validates its handoff.

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

/// A local source name that appears in diagnostics.
const SEED_NAME: &str = "local source";

pub fn run(
    index: &ArtifactIndex,
    request: &crate::Options,
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

pub fn run_with(
    launcher: &dyn zup_windows::Launcher,
    index: &ArtifactIndex,
    request: &crate::Options,
    scope: zup_core::SelectedScope,
    subsystem_gui: bool,
) -> Outcome {
    if matches!(request.verb, crate::Verb::Update | crate::Verb::Uninstall) {
        return Outcome::Refused {
            detail: "use the installed maintenance entry for update or uninstall".into(),
        };
    }
    let Some(trust) = index.artifact.trust.clone() else {
        return Outcome::Refused {
            detail: "this installer carries no trust block".to_owned(),
        };
    };
    if let Err(error) = trust.scope.select(Some(scope)) {
        return Outcome::Refused {
            detail: error.to_string(),
        };
    }
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

struct ThinPath<'a> {
    launcher: &'a dyn zup_windows::Launcher,
    trust: OnlineTrust,
    request: &'a crate::Options,
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
    let runtime_path = handoff_root.join(zup_transaction::MAINTENANCE_RUNTIME_DIRECTORY);
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
        target: resolved.variant.target,
        frontend: resolved.variant.frontend.clone(),
        mode: match request.verb {
            crate::Verb::Install => HandoffMode::Install,
            crate::Verb::Upgrade => HandoffMode::Upgrade,
            crate::Verb::Modify => HandoffMode::Modify,
            crate::Verb::Repair => HandoffMode::Repair,
            crate::Verb::Update | crate::Verb::Uninstall => {
                unreachable!("refused before acquisition")
            }
        },
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
        &resolver.context().state_root,
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
    super::classify(code)
}

fn handoff_arguments(
    state_root: &Path,
    cache_root: &Path,
    handoff_path: &Path,
    handoff_digest: Option<zup_core::Sha256Digest>,
    request: &crate::Options,
    scope: zup_core::SelectedScope,
    subsystem_gui: bool,
) -> Vec<String> {
    let mut arguments = crate::staged_arguments(request, state_root, scope);
    arguments.extend([
        "--acquired".to_owned(),
        cache_root.display().to_string(),
        "--handoff".to_owned(),
        handoff_path.display().to_string(),
    ]);
    if let Some(digest) = handoff_digest {
        arguments.push("--handoff-digest".to_owned());
        arguments.push(digest.to_hex());
    }
    if subsystem_gui
        && !request.non_interactive
        && !request.yes
        && !request.machine_readable
        && request.output.is_none()
    {
        arguments.push("--ui".to_owned());
    }
    if request.machine_readable && request.output.is_none() {
        arguments.push("--output".to_owned());
        arguments.push("jsonl".to_owned());
    }
    if let Some(source) = &request.source {
        arguments.push("--source".to_owned());
        arguments.push(source.display().to_string());
    }
    arguments
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
        assert_eq!(crate::classify(0), Outcome::Completed { code: 0 });
        assert_eq!(
            crate::classify(3010),
            Outcome::RebootRequired { code: 3010 }
        );
        assert_eq!(crate::classify(255), Outcome::InstallerFailed { code: 255 });
        assert_eq!(crate::classify(7), Outcome::RecoveryRequired { code: 7 });
        assert_eq!(crate::classify(1), Outcome::InstallerFailed { code: 1 });
        assert_eq!(
            Outcome::RebootRequired { code: 3010 }.describe(),
            "the installation needs a restart before it can finish (exit code 3010)"
        );
    }

    #[test]
    fn the_handoff_passes_locations_and_nothing_else() {
        let arguments = handoff_arguments(
            std::path::Path::new(r"C:\state"),
            std::path::Path::new(r"C:\state\content"),
            std::path::Path::new(r"C:\state\content\handoff\abc.json"),
            Some(zup_core::Sha256Digest::from_bytes([1; 32])),
            &crate::Options {
                source: Some(std::path::PathBuf::from(r"X:\Mirror")),
                state_root: None,
                machine_readable: true,
                verb: crate::Verb::Repair,
                enable: vec!["documentation".into()],
                force_files: true,
                ..Default::default()
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
        assert_eq!(arguments.first().map(String::as_str), Some("repair"));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--enable", "documentation"])
        );
        assert!(arguments.contains(&"--force-files".to_owned()));
        assert!(!arguments.contains(&"--ui".to_owned()));
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
            (zup_core::InstallScope::User, "user"),
            (zup_core::InstallScope::Machine, "machine"),
        ] {
            let scope = trust.select(None).unwrap();
            let arguments = handoff_arguments(
                std::path::Path::new(r"C:\state"),
                std::path::Path::new(r"C:\state\content"),
                std::path::Path::new(r"C:\state\content\handoff\abc.json"),
                Some(zup_core::Sha256Digest::from_bytes([1; 32])),
                &crate::Options {
                    source: None,
                    state_root: None,
                    machine_readable: false,
                    ..Default::default()
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
