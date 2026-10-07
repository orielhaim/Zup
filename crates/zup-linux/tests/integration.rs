#![cfg(target_os = "linux")]

//! Linux user-space desktop integration, end to end through the real engine.
//!
//! Each test composes a genuine installer carrying launchers, a protocol, a
//! file association, and icon files, then runs it through [`run`] with an
//! isolated home: install, upgrade, repair, uninstall, rollback, and symlink
//! attacks. Nothing here calls the executor directly; the path is the same one
//! a downloaded installer takes, from carrier open to ledger publish.
//!
//! Refresh tooling: `update-mime-database` is the real system tool. A fake
//! `update-desktop-database` stands in where the test machine has none, and
//! failing fakes prove the rollback invariant. `desktop-file-validate`
//! additionally validates generated entries wherever it is installed.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};

use zup_core::{AppId, SelectedScope};
use zup_linux::{LinuxAction, LinuxLedgerStore, LinuxOutcome, run};

use support::{
    FakeTools, IsolatedUser, compose_integration_fixture, data_home, fixture_association,
    fixture_protocol, menu_launcher, run_installer, run_tool, v1_files, v1_icons, v1_integration,
    v2_files, v2_icons,
};

fn applications() -> PathBuf {
    data_home().join("applications")
}

fn mime_packages() -> PathBuf {
    data_home().join("mime").join("packages")
}

fn icons() -> PathBuf {
    data_home().join("icons")
}

fn read(name: &Path) -> String {
    std::fs::read_to_string(name).unwrap_or_else(|_| panic!("{} exists", name.display()))
}

fn install_v1(user: &IsolatedUser, scratch: &Path) -> PathBuf {
    let (launchers, protocols, associations) = v1_integration();
    let installer = compose_integration_fixture(
        scratch,
        "v1",
        "1.0.0",
        &v1_files(),
        &launchers,
        &protocols,
        &associations,
        &v1_icons(),
    );
    let outcome = run_installer(&installer, &user.state, LinuxAction::Apply);
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "a fresh install commits: {outcome:?}"
    );
    installer
}

/// A fresh install writes deterministic integration resources: a visible
/// launcher, hidden URI and file handler entries, a MIME package, and hicolor
/// icons, and the shared databases regenerate from those sources.
#[test]
fn install_creates_desktop_integration() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    let desktop_log = scratch.path().join("desktop.log");
    tools.recording("update-desktop-database", &desktop_log);
    install_v1(&user, scratch.path());

    let install = user.programs().join("tool");
    let launcher = read(&applications().join("com.example.tool.desktop"));
    assert!(launcher.contains("[Desktop Entry]\n"), "{launcher}");
    assert!(launcher.contains("Type=Application\n"), "{launcher}");
    assert!(launcher.contains("Name=Tool\n"), "{launcher}");
    assert!(
        launcher.contains(&format!("Exec={}\n", install.join("tool").display())),
        "{launcher}"
    );
    assert!(launcher.contains("Icon=com.example.tool\n"), "{launcher}");
    assert!(!launcher.contains("MimeType="), "{launcher}");
    assert!(!launcher.contains("NoDisplay"), "{launcher}");

    let uri = read(&applications().join("com.example.tool-uri.desktop"));
    assert!(uri.contains("NoDisplay=true\n"), "{uri}");
    assert!(uri.contains("MimeType=x-scheme-handler/acme;\n"), "{uri}");
    assert!(uri.contains("--url %u"), "{uri}");

    let files = read(&applications().join("com.example.tool-files.desktop"));
    assert!(files.contains("NoDisplay=true\n"), "{files}");
    assert!(
        files.contains("MimeType=application/x-com.example.tool-foo;\n"),
        "{files}"
    );
    assert!(files.contains("%f"), "{files}");

    let package = read(&mime_packages().join("com.example.tool.xml"));
    assert!(
        package.contains("application/x-com.example.tool-foo"),
        "{package}"
    );
    assert!(package.contains("*.foo"), "{package}");
    assert!(package.contains("Foo document"), "{package}");

    assert_eq!(
        std::fs::read(icons().join("hicolor/48x48/apps/com.example.tool.png")).expect("icon"),
        b"icon-48-v1"
    );
    assert_eq!(
        std::fs::read(icons().join("hicolor/scalable/apps/com.example.tool.svg")).expect("svg"),
        b"<svg>icon-v1</svg>"
    );

    // The real MIME database regenerated from the package source.
    let globs = read(&data_home().join("mime/globs2"));
    assert!(
        globs.contains("application/x-com.example.tool-foo"),
        "{globs}"
    );
    // The desktop database refresh ran against the applications directory.
    let logged = read(&desktop_log);
    assert!(
        logged.contains(&applications().display().to_string()),
        "{logged}"
    );

    // An available validator additionally accepts every generated entry.
    if which("desktop-file-validate").is_some() {
        for entry in [
            "com.example.tool.desktop",
            "com.example.tool-uri.desktop",
            "com.example.tool-files.desktop",
        ] {
            let output = std::process::Command::new("desktop-file-validate")
                .arg(applications().join(entry))
                .output()
                .expect("the validator runs");
            assert!(
                output.status.success(),
                "{entry}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let ledger = LinuxLedgerStore::new(&user.state)
        .load(
            &AppId::new("com.example.tool").expect("an id"),
            SelectedScope::User,
        )
        .expect("the ledger reads")
        .expect("an installation is recorded");
    assert!(ledger.resources.len() >= 8, "integration is owned");
}

/// The generated command semantics execute: the installed tool opens the URI
/// and the file exactly as the lowered `Exec=` lines promise.
#[test]
fn handler_invocation_uses_lowered_arguments() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    tools.recording(
        "update-desktop-database",
        &scratch.path().join("desktop.log"),
    );
    install_v1(&user, scratch.path());

    let tool = user.programs().join("tool").join("tool");
    assert_eq!(
        run_tool(&tool, &["--url", "acme:document"]).trim(),
        "opened acme:document"
    );
    let document = scratch.path().join("note.foo");
    std::fs::write(&document, b"fixture").expect("a document");
    assert_eq!(
        run_tool(&tool, &[&document.to_string_lossy()]).trim(),
        format!("opened {}", document.display())
    );
}

/// An upgrade keeps stable identities while changing content: the renamed
/// launcher updates in place, the retired file type's MIME definition goes,
/// the new type arrives, and the icon set turns over.
#[test]
fn upgrade_updates_and_retires_integration() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    let desktop_log = scratch.path().join("desktop.log");
    tools.recording("update-desktop-database", &desktop_log);
    install_v1(&user, scratch.path());

    let unrelated = applications().join("unrelated.desktop");
    std::fs::write(
        &unrelated,
        b"[Desktop Entry]\nType=Application\nName=Other\n",
    )
    .expect("neighbor");

    let installer = compose_integration_fixture(
        scratch.path(),
        "v2",
        "1.1.0",
        &v2_files(),
        &[menu_launcher("Tool Renamed")],
        &[fixture_protocol("acme")],
        &[fixture_association(".bar", "acme.bar", "Bar document")],
        &v2_icons(),
    );
    let outcome = run_installer(&installer, &user.state, LinuxAction::Apply);
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "an upgrade commits: {outcome:?}"
    );

    let launcher = read(&applications().join("com.example.tool.desktop"));
    assert!(launcher.contains("Name=Tool Renamed\n"), "{launcher}");
    assert!(
        !applications()
            .join("com.example.tool Renamed.desktop")
            .exists(),
        "no second entry appears beside the renamed one"
    );
    let package = read(&mime_packages().join("com.example.tool.xml"));
    assert!(!package.contains(".foo"), "{package}");
    assert!(
        package.contains("application/x-com.example.tool-bar"),
        "{package}"
    );
    let globs = read(&data_home().join("mime/globs2"));
    assert!(
        !globs.contains("application/x-com.example.tool-foo"),
        "{globs}"
    );
    assert!(
        globs.contains("application/x-com.example.tool-bar"),
        "{globs}"
    );
    let uri = read(&applications().join("com.example.tool-uri.desktop"));
    assert!(uri.contains("x-scheme-handler/acme"), "{uri}");
    assert!(
        !icons()
            .join("hicolor/48x48/apps/com.example.tool.png")
            .exists(),
        "the retired icon size is gone"
    );
    assert_eq!(
        std::fs::read(icons().join("hicolor/64x64/apps/com.example.tool.png")).expect("new icon"),
        b"icon-64-v2"
    );
    assert_eq!(
        std::fs::read(&unrelated).expect("neighbor"),
        b"[Desktop Entry]\nType=Application\nName=Other\n"
    );
    assert!(read(&desktop_log).contains(&applications().display().to_string()));
}

/// Repair restores missing owned integration automatically and damaged owned
/// integration with force, and regenerates the derived databases afterward.
#[test]
fn repair_restores_owned_integration() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    tools.recording(
        "update-desktop-database",
        &scratch.path().join("desktop.log"),
    );
    let installer = install_v1(&user, scratch.path());

    let desktop = applications().join("com.example.tool.desktop");
    let package = mime_packages().join("com.example.tool.xml");
    let icon = icons().join("hicolor/48x48/apps/com.example.tool.png");
    let before = read(&desktop);
    std::fs::remove_file(&desktop).expect("delete the entry");
    std::fs::remove_file(&package).expect("delete the package");

    let outcome = run(&zup_linux::LinuxRunRequest {
        installer: installer.clone(),
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Repair { force_files: false },
    })
    .expect("repair reaches a stable outcome");
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "missing files come back without force: {outcome:?}"
    );
    assert_eq!(read(&desktop), before);
    assert!(read(&package).contains("*.foo"));

    // A damaged-but-present icon is not silently overwritten: repair without
    // force refuses rather than deciding the user's bytes are wrong.
    std::fs::write(&icon, b"damaged").expect("damage the icon");
    let outcome = run(&zup_linux::LinuxRunRequest {
        installer: installer.clone(),
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Repair { force_files: false },
    });
    assert!(
        outcome.is_err(),
        "damage without force is refused, not repaired: {outcome:?}"
    );
    assert_eq!(std::fs::read(&icon).expect("icon"), b"damaged");

    // The damaged-but-present icon needs force: repair does not silently
    // decide the user's bytes are wrong.
    let outcome = run(&zup_linux::LinuxRunRequest {
        installer: installer.clone(),
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Repair { force_files: true },
    })
    .expect("forced repair reaches a stable outcome");
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "forced repair restores damage: {outcome:?}"
    );
    assert_eq!(std::fs::read(&icon).expect("icon"), b"icon-48-v1");
    assert!(read(&data_home().join("mime/globs2")).contains("application/x-com.example.tool-foo"));
}

/// Uninstall removes only Zup-owned integration: every unrelated neighbor
/// survives byte-for-byte and the shared databases reflect the removal.
#[test]
fn uninstall_removes_only_owned_integration() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    tools.recording(
        "update-desktop-database",
        &scratch.path().join("desktop.log"),
    );
    let installer = install_v1(&user, scratch.path());

    let neighbors = [
        applications().join("other.desktop"),
        icons().join("hicolor/48x48/apps/other.png"),
        mime_packages().join("other.xml"),
    ];
    for neighbor in &neighbors {
        if let Some(parent) = neighbor.parent() {
            std::fs::create_dir_all(parent).expect("a neighbor directory");
        }
        std::fs::write(neighbor, b"unrelated").expect("a neighbor");
    }
    let mimeapps = data_home().join("mimeapps.list");
    std::fs::write(
        &mimeapps,
        "[Added Associations]\ntext/plain=other.desktop;\n[Default Applications]\ntext/plain=other.desktop\n",
    )
    .expect("user preferences");
    let before: Vec<Vec<u8>> = neighbors
        .iter()
        .map(|path| std::fs::read(path).expect("a neighbor"))
        .collect();
    let preferences = read(&mimeapps);

    let outcome = run(&zup_linux::LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Uninstall,
    });
    assert!(
        matches!(outcome, Ok(LinuxOutcome::Committed { .. })),
        "uninstall commits: {outcome:?}"
    );
    assert!(!applications().join("com.example.tool.desktop").exists());
    assert!(!applications().join("com.example.tool-uri.desktop").exists());
    assert!(
        !applications()
            .join("com.example.tool-files.desktop")
            .exists()
    );
    assert!(!mime_packages().join("com.example.tool.xml").exists());
    assert!(
        !icons()
            .join("hicolor/48x48/apps/com.example.tool.png")
            .exists()
    );
    for (neighbor, bytes) in neighbors.iter().zip(before) {
        assert_eq!(std::fs::read(neighbor).expect("a neighbor survives"), bytes);
    }
    assert_eq!(
        read(&mimeapps),
        preferences,
        "user preferences are untouched"
    );
    assert!(
        applications().exists(),
        "shared directories are not removed"
    );
    assert!(
        mime_packages().exists(),
        "shared directories are not removed"
    );
    let globs = read(&data_home().join("mime/globs2"));
    assert!(
        !globs.contains("application/x-com.example.tool-foo"),
        "{globs}"
    );
}

/// A refresh failure after the files applied rolls everything back: the
/// authoritative sources are restored, the databases regenerate from the
/// restored state, and a later run with a working tool commits.
#[test]
fn failed_refresh_rolls_back_and_recovers() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    let flag = scratch.path().join("mime-ok");
    let mime_log = scratch.path().join("mime.log");
    let desktop_log = scratch.path().join("desktop.log");
    tools.flaky("update-mime-database", &flag, &mime_log);
    tools.recording("update-desktop-database", &desktop_log);

    let (launchers, protocols, associations) = v1_integration();
    let installer = compose_integration_fixture(
        scratch.path(),
        "v1",
        "1.0.0",
        &v1_files(),
        &launchers,
        &protocols,
        &associations,
        &v1_icons(),
    );
    let outcome = run(&zup_linux::LinuxRunRequest {
        installer: installer.clone(),
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
    })
    .expect("a failed refresh still reaches a stable outcome");
    assert!(
        matches!(outcome, LinuxOutcome::RolledBack),
        "a failing refresh rolls back: {outcome:?}"
    );
    assert!(
        !user.programs().join("tool").exists(),
        "the payload rolls back with the refresh"
    );
    assert!(
        !applications().join("com.example.tool.desktop").exists(),
        "integration rolls back with the refresh"
    );
    assert!(
        LinuxLedgerStore::new(&user.state)
            .load(
                &AppId::new("com.example.tool").expect("an id"),
                SelectedScope::User,
            )
            .expect("the ledger reads")
            .is_none(),
        "nothing is owned after rollback"
    );

    std::fs::write(&flag, b"ok").expect("repair the tool");
    let outcome = run_installer(&installer, &user.state, LinuxAction::Apply);
    assert!(
        matches!(outcome, LinuxOutcome::Committed { .. }),
        "the retry commits: {outcome:?}"
    );
    assert!(applications().join("com.example.tool.desktop").exists());
    assert!(read(&desktop_log).contains(&applications().display().to_string()));
}

/// A rollback after one refresh succeeded regenerates the derived databases
/// from the resulting world, including the world with no source left.
///
/// The MIME refresh applies, then the desktop refresh fails, so the
/// transaction rolls back and removes the final MIME source with it. The
/// post-rollback sweep must regenerate the MIME cache from the empty world:
/// the cache must not keep entries for a source that no longer exists.
#[test]
fn rollback_after_a_partial_refresh_regenerates_the_empty_world() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    // The MIME tool stays the real system one; only the desktop refresh is
    // forced to fail, after the MIME refresh has already applied.
    tools.tool("update-desktop-database", "#!/bin/sh\nexit 1\n");

    let (launchers, protocols, associations) = v1_integration();
    let installer = compose_integration_fixture(
        scratch.path(),
        "v1",
        "1.0.0",
        &v1_files(),
        &launchers,
        &protocols,
        &associations,
        &v1_icons(),
    );
    let outcome = run(&zup_linux::LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
    })
    .expect("a failed refresh still reaches a stable outcome");
    assert!(
        matches!(outcome, LinuxOutcome::RolledBack),
        "a failing desktop refresh rolls back: {outcome:?}"
    );
    assert!(
        !mime_packages().join("com.example.tool.xml").exists(),
        "the rolled-back source is gone"
    );
    // The authoritative world is now empty of Zup MIME sources, so the
    // derived cache must have been regenerated from that world rather than
    // left holding the removed source's entries.
    let globs = data_home().join("mime/globs2");
    assert!(
        globs.is_file(),
        "the sweep regenerates the cache from the empty world instead of skipping it"
    );
    assert!(
        !read(&globs).contains("application/x-com.example.tool-foo"),
        "no stale entry survives the rollback"
    );
}

/// A missing refresh tool fails before anything mutates: the sealed `PATH`
/// resolves neither database tool, so preflight refuses with the capability
/// named and the machine keeps no trace of the attempt.
#[test]
fn missing_tools_fail_before_mutation() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let tools = FakeTools::install();
    tools.seal();

    let (launchers, protocols, associations) = v1_integration();
    let installer = compose_integration_fixture(
        scratch.path(),
        "v1",
        "1.0.0",
        &v1_files(),
        &launchers,
        &protocols,
        &associations,
        &v1_icons(),
    );
    let outcome = run(&zup_linux::LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
    });
    let error = outcome.expect_err("a missing tool refuses the run");
    assert!(
        error.to_string().contains("update-mime-database")
            || error.to_string().contains("update-desktop-database"),
        "the diagnostic names the capability: {error}"
    );
    assert!(
        !user.programs().join("tool").exists(),
        "nothing installs before preflight passes"
    );
    assert!(
        !applications().join("com.example.tool.desktop").exists(),
        "no integration lands before preflight passes"
    );
}

/// Planted symlinks at integration destinations are refused before anything
/// is mutated: installation never writes through an attacker-controlled link.
#[test]
fn planted_symlinks_are_refused() {
    let user = IsolatedUser::isolate();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let _tools = FakeTools::install();
    let elsewhere = tempfile::tempdir().expect("an unrelated tree");
    std::fs::write(elsewhere.path().join("target"), b"elsewhere").expect("write");

    for destination in [
        applications().join("com.example.tool.desktop"),
        mime_packages().join("com.example.tool.xml"),
        icons().join("hicolor/48x48/apps/com.example.tool.png"),
    ] {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).expect("a parent");
        }
        std::os::unix::fs::symlink(elsewhere.path().join("target"), &destination)
            .expect("a planted link");
    }

    let (launchers, protocols, associations) = v1_integration();
    let installer = compose_integration_fixture(
        scratch.path(),
        "v1",
        "1.0.0",
        &v1_files(),
        &launchers,
        &protocols,
        &associations,
        &v1_icons(),
    );
    let outcome = run(&zup_linux::LinuxRunRequest {
        installer,
        scope: SelectedScope::User,
        state_root: Some(user.state.clone()),
        action: LinuxAction::Apply,
    });
    assert!(outcome.is_err(), "a planted link refuses: {outcome:?}");
    assert_eq!(
        std::fs::read(elsewhere.path().join("target")).expect("untouched"),
        b"elsewhere"
    );
    assert!(
        !user.programs().join("tool").join("tool").exists(),
        "nothing installs past a refused destination"
    );
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| {
                candidate.is_file()
                    && std::os::unix::fs::PermissionsExt::mode(
                        &std::fs::metadata(candidate).expect("stat").permissions(),
                    ) & 0o111
                        != 0
            })
    })
}
