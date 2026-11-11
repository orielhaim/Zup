#![cfg(all(windows, feature = "gui", feature = "online"))]

mod support {
    pub mod project;
    pub mod update_repository;
}

use support::project::{AppSpec, Payload, State};
use support::update_repository::{ROOT, RepositoryServer};
use zup_core::{Frontend, SelectedScope};
use zup_session::{SessionHost, SessionOutcome};

#[test]
fn native_session_updates_preserve_selection_and_report_real_results() {
    let scratch = tempfile::tempdir().unwrap();
    let app = AppSpec::unique("session-update");
    let state = State::new();
    let _cleanup = support::project::cleanup(&app, &state);
    let repository = scratch.path().join("repository");
    let server = RepositoryServer::start(repository.clone());
    let mut plan = support::project::plan(
        &app,
        Frontend::Headless,
        &[
            Payload::named("core.txt", b"initial", Some("core")),
            Payload::named("docs.txt", b"documentation", Some("docs")),
        ],
        &[],
        scratch.path(),
    );
    plan.installer.updates = Some(zup_core::UpdateConfig {
        repository: server.url.clone(),
        channel: "stable".into(),
        trusted_root: ROOT.to_vec(),
    });
    let setup = scratch.path().join("setup.exe");
    zup_compose::compose(
        &plan.installer.target,
        &support::project::template(Frontend::Headless),
        &setup,
        &zup_bundle::BundleWriter::encode(&plan, &[]).unwrap(),
        None,
    )
    .unwrap();
    support::project::succeed(
        &setup,
        &state,
        "install",
        &["--disable", "docs", "--yes", "--output", "json"],
    );
    plan.installer.app.version = "2.0.0".parse().unwrap();
    let core = &mut plan.files[0];
    std::fs::write(&core.source, b"updated").unwrap();
    core.size = 7;
    core.sha256 = zup_core::hash_bytes(b"updated");
    plan.total_size = plan.files.iter().map(|file| file.size).sum();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (release, _) = runtime.block_on(support::update_repository::publish(
        &repository,
        &plan,
        1,
        &support::project::template(Frontend::Headless),
        None,
    ));
    let mut host = zup_installer::host::WindowsSessionHost::default();
    let installed_build = host.describe(&setup).unwrap();
    let context = host
        .observe(
            &installed_build.installer,
            SelectedScope::User,
            Some(state.path()),
        )
        .unwrap();
    let control = zup_runtime::OperationControl::new(zup_runtime::ExecutionPolicy::NonInteractive);
    let mut statuses = Vec::new();
    assert_eq!(
        host.update(
            &context,
            &zup_runtime::UpdateRequest::Check,
            &control,
            &mut |status| statuses.push(status)
        )
        .unwrap(),
        SessionOutcome::UpdateChecked
    );
    assert!(
        statuses
            .iter()
            .any(|status| matches!(status, zup_preset_protocol::UpdateState::Available { .. }))
    );
    let expected = zup_core::ReleaseCandidate {
        version: "2.0.0".parse().unwrap(),
        release: release.release_digest,
    };
    let stale = zup_core::ReleaseCandidate {
        version: "2.0.0".parse().unwrap(),
        release: zup_core::hash_bytes(b"stale"),
    };
    assert!(
        host.update(
            &context,
            &zup_runtime::UpdateRequest::Install {
                expected: Some(stale)
            },
            &control,
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(app.install_directory().join("core.txt")).unwrap(),
        b"initial"
    );
    let cancelled =
        zup_runtime::OperationControl::new(zup_runtime::ExecutionPolicy::NonInteractive);
    cancelled.cancellation().cancel();
    assert_eq!(
        host.update(
            &context,
            &zup_runtime::UpdateRequest::Check,
            &cancelled,
            &mut |_| {}
        )
        .unwrap(),
        SessionOutcome::Deployment(zup_runtime::InstallOutcome::Cancelled)
    );
    assert_eq!(
        host.update(
            &context,
            &zup_runtime::UpdateRequest::Install {
                expected: Some(expected)
            },
            &control,
            &mut |status| statuses.push(status)
        )
        .unwrap(),
        SessionOutcome::Deployment(zup_runtime::InstallOutcome::Committed)
    );
    let record = zup_windows::InstallationRecordStore::new(state.path())
        .load(&plan.installer.app.id, SelectedScope::User)
        .unwrap()
        .unwrap();
    assert_eq!(record.current.version.to_string(), "2.0.0");
    assert_eq!(
        record
            .current
            .selected_components
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        vec!["core"]
    );
    assert_eq!(
        std::fs::read(app.install_directory().join("core.txt")).unwrap(),
        b"updated"
    );
    assert!(!app.install_directory().join("docs.txt").exists());
    support::project::succeed(
        &state.maintenance(&AppSpec {
            version: "2.0.0".into(),
            ..app.clone()
        }),
        &state,
        "uninstall",
        &["--yes", "--output", "json"],
    );
}
