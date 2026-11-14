use super::*;
#[path = "update_repository.rs"]
mod repository;
use repository::{ROOT, RepositoryServer};

pub(super) fn preset(frontend: Frontend) -> Option<Vec<u8>> {
    if frontend != Frontend::Gui {
        return None;
    }
    #[cfg(feature = "gui")]
    {
        Some(
            std::fs::read(
                Path::new(env!("CARGO_BIN_EXE_zup-setup-gui"))
                    .parent()
                    .unwrap()
                    .join("test-gui-install-preset"),
            )
            .expect("the native GUI test driver is built with zup-linux test-support"),
        )
    }
    #[cfg(not(feature = "gui"))]
    {
        unreachable!()
    }
}

fn runtime(frontend: Frontend) -> &'static Path {
    Path::new(match frontend {
        Frontend::Console => env!("CARGO_BIN_EXE_zup-setup-console"),
        Frontend::Headless => env!("CARGO_BIN_EXE_zup-setup-headless"),
        #[cfg(feature = "gui")]
        Frontend::Gui => env!("CARGO_BIN_EXE_zup-setup-gui"),
        #[cfg(not(feature = "gui"))]
        _ => unreachable!(),
    })
}

fn compose(plan: &TargetBuildPlan, output: &Path) {
    let bytes = zup_bundle::BundleWriter::encode(plan, &[]).unwrap();
    zup_compose::compose(
        &plan.installer.target,
        runtime(plan.installer.frontend),
        output,
        &bytes,
        preset(plan.installer.frontend).as_deref(),
    )
    .unwrap();
}

#[rstest::rstest]
#[case(Frontend::Headless)]
#[case(Frontend::Console)]
#[cfg_attr(feature = "gui", case(Frontend::Gui))]
fn authenticated_updates_preserve_choices_and_support_offline_maintenance(
    #[case] frontend: Frontend,
) {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut fixture = Fixture::new(frontend);
    if frontend == Frontend::Gui {
        fixture
            .plan
            .installer
            .components
            .iter_mut()
            .find(|component| component.id.as_str() == "docs")
            .unwrap()
            .default = true;
    }
    let repository = fixture.root.path().join("repository");
    let server = RepositoryServer::start(repository.clone());
    fixture.plan.installer.updates = Some(zup_core::UpdateConfig {
        repository: server.url.clone(),
        channel: "stable".into(),
        trusted_root: ROOT.to_vec(),
    });
    compose(&fixture.plan, &fixture.setup);
    let install = if frontend == Frontend::Gui {
        Command::new(&fixture.setup)
            .env("DISPLAY", "test-display")
            .env("XDG_STATE_HOME", fixture.root.path())
            .args(["--scope", "user"])
            .arg("--state-root")
            .arg(fixture.root.path().join("zup"))
            .output()
            .unwrap()
    } else {
        fixture
            .command()
            .args([
                "install", "--scope", "user", "--enable", "docs", "--output", "json",
            ])
            .output()
            .unwrap()
    };
    assert!(
        install.status.success(),
        "{frontend}: {} {}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );
    let first_ledger = zup_linux::LinuxInstallationStore::new(fixture.root.path().join("zup"))
        .load(
            &fixture.plan.installer.app.id,
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    let old_generation = std::path::PathBuf::from(
        first_ledger
            .current
            .maintenance
            .as_ref()
            .unwrap()
            .directory()
            .as_str(),
    );
    let foreign = old_generation.parent().unwrap().join("maintenance.foreign");
    std::fs::write(&foreign, b"keep this file").unwrap();
    let updated_version = if frontend == Frontend::Headless {
        "1.0.0"
    } else {
        "2.0.0"
    };
    fixture.plan.installer.app.version = updated_version.parse().unwrap();
    let core = fixture
        .plan
        .files
        .iter_mut()
        .find(|file| file.component.as_ref().unwrap().as_str() == "core")
        .unwrap();
    std::fs::write(&core.source, b"updated core").unwrap();
    core.size = 12;
    core.sha256 = zup_core::hash_bytes(b"updated core");
    fixture.plan.total_size = fixture.plan.files.iter().map(|file| file.size).sum();
    let (release, seed) = tokio.block_on(repository::publish(
        &repository,
        &fixture.plan,
        1,
        runtime(frontend),
        preset(frontend),
    ));
    let check = fixture
        .command()
        .args(["update", "check", "--scope", "user", "--output", "json"])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{} {}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    let result: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains(updated_version)
    );
    assert_eq!(
        std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
        b"core"
    );
    if frontend == Frontend::Headless {
        use std::io::{BufRead, BufReader};
        let mut interrupted = fixture
            .command()
            .args([
                "update", "--scope", "user", "--yes", "--output", "jsonl", "--source",
            ])
            .arg(&seed)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(interrupted.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "acquisition must report progress before finishing"
            );
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["type"] == "progress" {
                break;
            }
        }
        assert!(
            Command::new("kill")
                .args(["-INT", &interrupted.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let mut remainder = String::new();
        stdout.read_to_string(&mut remainder).unwrap();
        assert_eq!(interrupted.wait().unwrap().code(), Some(2), "{remainder}");
        assert_eq!(
            std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
            b"core"
        );
    }
    let digest = fixture
        .plan
        .files
        .iter()
        .find(|file| file.component.as_ref().unwrap().as_str() == "core")
        .unwrap()
        .sha256
        .to_hex();
    let blob = seed.join(format!("blobs/sha256/{}/{}", &digest[..2], &digest[2..]));
    zup_acquire::ContentCache::open(
        fixture.root.path().join("zup/content"),
        zup_acquire::CachePolicy::Auto,
    )
    .unwrap()
    .forget_digest(&digest.parse::<zup_core::Sha256Digest>().unwrap())
    .unwrap();
    std::fs::write(&blob, b"hostile core").unwrap();
    let refused = fixture
        .command()
        .args([
            "update", "--scope", "user", "--yes", "--output", "json", "--source",
        ])
        .arg(&seed)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    serde_json::from_slice::<Value>(&refused.stdout).unwrap();
    assert_eq!(
        std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
        b"core"
    );
    std::fs::write(&blob, b"updated core").unwrap();
    if frontend == Frontend::Console {
        use zup_session::SessionHost;
        let mut host = zup_linux::LinuxSessionHost::open(fixture.setup.clone());
        let context = host
            .observe(
                &fixture.plan.installer,
                zup_core::SelectedScope::User,
                Some(&fixture.root.path().join("zup")),
            )
            .unwrap();
        let mut states = Vec::new();
        let rejected = host.update(
            &context,
            &zup_runtime::UpdateRequest::Install {
                expected: Some(zup_core::ReleaseCandidate {
                    version: updated_version.parse().unwrap(),
                    release: zup_core::Sha256Digest::from_bytes([0; 32]),
                }),
            },
            &zup_runtime::OperationControl::new(zup_runtime::ExecutionPolicy::NonInteractive),
            &mut |state| states.push(state),
        );
        assert!(rejected.is_err());
        assert_eq!(
            std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
            b"core"
        );
        let outcome = host
            .update(
                &context,
                &zup_runtime::UpdateRequest::Install {
                    expected: Some(zup_core::ReleaseCandidate {
                        version: updated_version.parse().unwrap(),
                        release: release.release_digest,
                    }),
                },
                &zup_runtime::OperationControl::new(zup_runtime::ExecutionPolicy::NonInteractive),
                &mut |state| states.push(state),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            zup_session::SessionOutcome::Deployment(zup_runtime::InstallOutcome::Committed)
        ));
        assert!(!states.is_empty());
    }
    if frontend == Frontend::Gui {
        let updated = Command::new(&fixture.setup)
            .env("DISPLAY", "test-display")
            .env("XDG_STATE_HOME", fixture.root.path())
            .env("ZUP_TEST_UPDATE", "1")
            .args(["--scope", "user"])
            .arg("--state-root")
            .arg(fixture.root.path().join("zup"))
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{} {}",
            String::from_utf8_lossy(&updated.stdout),
            String::from_utf8_lossy(&updated.stderr)
        );
    }
    let update = fixture
        .command()
        .args([
            "update", "--scope", "user", "--yes", "--output", "jsonl", "--source",
        ])
        .arg(&seed)
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{} {}",
        String::from_utf8_lossy(&update.stdout),
        String::from_utf8_lossy(&update.stderr)
    );
    let events: Vec<Value> = String::from_utf8(update.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "completed")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
        b"updated core"
    );
    assert!(fixture.root.path().join("application/docs.txt").exists());
    let state = fixture.root.path().join("zup");
    let ledger = zup_linux::LinuxInstallationStore::new(&state)
        .load(
            &fixture.plan.installer.app.id,
            zup_core::SelectedScope::User,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        ledger.release_identity().unwrap().release,
        release.release_digest
    );
    assert!(
        ledger
            .current
            .selected_components
            .iter()
            .any(|id| id.as_str() == "docs")
    );
    let maintenance = std::path::PathBuf::from(
        ledger
            .current
            .maintenance
            .as_ref()
            .unwrap()
            .directory()
            .as_str(),
    )
    .join("maintenance");
    for transaction in [
        &first_ledger.current.transaction,
        &ledger.current.transaction,
    ] {
        let recovered = fixture
            .command()
            .args([
                "recover",
                "--transaction",
                &transaction.to_string(),
                "--scope",
                "user",
                "--output",
                "json",
            ])
            .output()
            .unwrap();
        assert!(
            recovered.status.success(),
            "{} {}",
            String::from_utf8_lossy(&recovered.stdout),
            String::from_utf8_lossy(&recovered.stderr)
        );
        serde_json::from_slice::<Value>(&recovered.stdout).unwrap();
    }
    assert_eq!(std::fs::read(&foreign).unwrap(), b"keep this file");
    assert_eq!(
        std::fs::read(fixture.root.path().join("application/core.txt")).unwrap(),
        b"updated core"
    );
    std::fs::remove_dir_all(&repository).unwrap();
    std::fs::write(fixture.root.path().join("application/core.txt"), b"drifted").unwrap();
    for args in [
        vec!["repair", "--force"],
        vec!["modify", "--disable", "docs"],
        vec!["uninstall"],
    ] {
        let output = Command::new(&maintenance)
            .args(args)
            .args(["--scope", "user", "--output", "json", "--state-root"])
            .arg(&state)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap();
    }
    assert!(!fixture.root.path().join("application/core.txt").exists());
}
