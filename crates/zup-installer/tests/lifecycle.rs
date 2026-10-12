#![cfg(windows)]

//! The lifecycle an end user performs, end to end against a real installer.
//!
//! Everything here runs a composed `Setup.exe` - the file a person double-clicks -
//! and then the `maintenance.exe` the installation persists. What is proved is the
//! part a user and an operator depend on: the verbs exist and succeed, the
//! installation survives its own source tree being deleted, Apps & Features points
//! at the persisted copy, a drifted registry entry is left alone, and a failed
//! upgrade leaves the previous installation committed and usable.
//!
//! Every test uses a private state root and a fresh application identifier, so
//! nothing contends with a real installation or with another test.

#![cfg(windows)]

use std::{collections::BTreeMap, fs};

use zup_core::{AppId, SelectedScope};
use zup_transaction::InstallationLock;
use zup_windows::{AppsFeaturesValue, InstallLedgerStore};

#[path = "support/project.rs"]
mod project;

use project::{AppSpec, Payload, State, cleanup, compose, succeed};

const FRONTEND: zup_core::Frontend = zup_core::Frontend::Gui;

/// An application with a core component and an optional documentation component,
/// which is the shape a repair and a modify need to be meaningful.
fn application(label: &str) -> (AppSpec, Vec<Payload>) {
    let app = AppSpec::unique(label);
    let payload = vec![
        Payload::named("app.exe", b"app payload", Some("core")),
        Payload::named("readme.txt", b"optional payload", Some("docs")),
    ];
    (app, payload)
}

/// The Apps & Features values for one application, read the way a machine would.
fn registered(app: &AppSpec) -> BTreeMap<String, AppsFeaturesValue> {
    zup_windows::inspect_uninstall_registration(SelectedScope::User, &app.id)
        .expect("the registration is readable")
        .unwrap_or_else(|| panic!("`{}` is registered", app.id))
        .values
}

fn registered_string(values: &BTreeMap<String, AppsFeaturesValue>, name: &str) -> String {
    let AppsFeaturesValue::String(value) = &values[name] else {
        panic!("{name} is not REG_SZ");
    };
    value.clone()
}

/// The version the ledger records for one application, or `None` when the
/// application is not installed.
fn ledger_version(state: &State, app: &AppSpec) -> Option<String> {
    InstallLedgerStore::new(state.path())
        .load(
            &AppId::new(&app.id).expect("an app id"),
            SelectedScope::User,
        )
        .expect("the ledger is readable")
        .map(|ledger| ledger.version.to_string())
}

/// Remove an application and everything it left behind, so a test that fails
/// half way through does not leave an install directory or a registry key on the
/// developer's machine.
#[test]
fn an_installation_outlives_its_own_source_tree_and_stays_operable() {
    let (app, payload) = application("lifecycle");
    let state = State::new();
    let _cleanup = cleanup(&app, &state);
    let installer = compose(&app, FRONTEND, &payload);
    let source_tree = installer
        .path()
        .parent()
        .expect("a directory")
        .to_path_buf();
    let install = app.install_directory();

    installer.succeed(&state, "install", &[]);

    // The installation persisted its own copy of the runtime, and named it for
    // what it is: a maintenance tool, not the setup a person downloaded.
    let maintenance = state.maintenance(&app);
    assert!(maintenance.is_file(), "{}", maintenance.display());
    assert_eq!(
        maintenance.file_name().and_then(|name| name.to_str()),
        Some("maintenance.exe"),
        "the persisted copy is the maintenance executable, not the downloaded setup"
    );

    // Apps & Features is the only place a person or an installer inventory on this
    // machine learns the application exists, so all of its executable paths point
    // at the persisted copy. The setup file is a download: a user who deletes it
    // must not delete their own uninstaller.
    let values = registered(&app);
    assert_eq!(
        values["DisplayName"],
        AppsFeaturesValue::String(app.name.clone())
    );
    assert_eq!(
        values["DisplayVersion"],
        AppsFeaturesValue::String(app.version.clone())
    );
    assert!(
        matches!(values["EstimatedSize"], AppsFeaturesValue::Dword(size) if size > 0),
        "{:?}",
        values["EstimatedSize"]
    );
    // Compared as the file the path resolves to, not as the text of the path.
    // A path has more than one spelling - a checkout or an environment variable
    // may hand out the 8.3 short form of a directory whose long form the runtime
    // then writes, because the runtime resolves the state root before recording
    // it - and Apps & Features is named by what the path *is*. Windows resolves
    // both spellings to the one file, so a test that compares strings reports a
    // defect that is not there.
    let persisted = fs::canonicalize(&maintenance).expect("the persisted copy resolves");
    let persisted = zup_windows::plain_path_text(&persisted);
    for name in ["UninstallString", "ModifyPath", "DisplayIcon"] {
        let value = registered_string(&values, name);
        assert!(
            value.contains(&persisted),
            "{name} does not point at the persisted maintenance copy: {value}"
        );
    }

    // The whole point: the download and the source tree are both gone, and the
    // installation is still fully operable.
    fs::remove_file(installer.path()).expect("the setup file is removable");
    fs::remove_dir_all(&source_tree).expect("the source tree is removable");
    assert_eq!(fs::read(install.join("app.exe")).unwrap(), b"app payload");

    // `repair` puts back what a person or a cleaner deleted.
    fs::remove_file(install.join("app.exe")).expect("the payload is removable");
    succeed(&maintenance, &state, "repair", &[]);
    assert_eq!(fs::read(install.join("app.exe")).unwrap(), b"app payload");

    // `modify` changes which components are installed, without reinstalling.
    succeed(&maintenance, &state, "modify", &["--disable", "docs"]);
    assert!(!install.join("readme.txt").exists());
    succeed(&maintenance, &state, "modify", &["--enable", "docs"]);
    assert_eq!(
        fs::read(install.join("readme.txt")).unwrap(),
        b"optional payload"
    );

    succeed(&maintenance, &state, "uninstall", &[]);
    assert!(!install.join("app.exe").exists());
    assert!(!install.exists(), "the install directory is gone");
    assert!(
        zup_windows::inspect_uninstall_registration(SelectedScope::User, &app.id)
            .expect("the registration is readable")
            .is_none(),
        "Apps & Features no longer lists the application"
    );
    assert!(!maintenance.exists());
    assert!(!state.path().join("maintenance").join(&app.id).exists());
    assert_eq!(ledger_version(&state, &app), None);

    // An uninstall leaves no state root debris behind. A transaction that had to
    // be cleaned up on the next run would be a transaction a user pays for.
    for residue in ["transactions", "work", "installations"] {
        assert!(
            !state.path().join(residue).exists(),
            "{residue} survived the uninstall"
        );
    }
    let lock_key = InstallationLock::lock_key(&app.id, "user");
    assert!(!state.path().join(format!("{lock_key}.lock")).exists());
}

#[test]
fn an_uninstall_leaves_an_apps_and_features_entry_it_did_not_write() {
    // A person who edits an entry in Apps & Features - a custom uninstall string,
    // a renamed entry - has expressed an intent. Deleting their edit is a data
    // loss, and the entry outlives the application.
    let (app, payload) = application("drift");
    let state = State::new();
    let _cleanup = cleanup(&app, &state);
    let installer = compose(&app, FRONTEND, &payload);
    installer.succeed(&state, "install", &[]);
    let maintenance = state.maintenance(&app);
    assert!(maintenance.is_file());

    let key = format!(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{}",
        app.id
    );
    windows_registry::CURRENT_USER
        .options()
        .read()
        .write()
        .open(&key)
        .expect("the registration is open")
        .set_string("DisplayName", "Changed outside zup")
        .expect("the entry is writable");

    succeed(&maintenance, &state, "uninstall", &[]);

    let remaining = zup_windows::inspect_uninstall_registration(SelectedScope::User, &app.id)
        .expect("the registration is readable")
        .expect("the drifted entry survives");
    assert_eq!(
        remaining.values["DisplayName"],
        AppsFeaturesValue::String("Changed outside zup".into())
    );
    // Everything zup did own is gone even so.
    assert!(!maintenance.exists());
    assert_eq!(ledger_version(&state, &app), None);
}

#[test]
fn a_failed_upgrade_leaves_the_previous_installation_committed_and_usable() {
    // An upgrade that cannot complete must not leave the machine half-upgraded.
    // The previous version stays committed, stays registered, and is still the
    // thing that can repair the installation.
    let (app, payload) = application("upgrade");
    let next = app.at_version("2.0.0");
    let state = State::new();
    let _cleanup = cleanup(&app, &state);
    let install = app.install_directory();
    let v1 = compose(&app, FRONTEND, &payload);
    v1.succeed(&state, "install", &[]);
    let maintenance_v1 = state.maintenance(&app);
    assert!(maintenance_v1.is_file());
    assert_eq!(ledger_version(&state, &app).as_deref(), Some("1.0.0"));
    // The downloaded setup for the old version is not part of the installation.
    drop(fs::remove_file(v1.path()));

    // The next version installs a file the next one cannot write, so its install
    // fails part way through.
    let blocked = vec![
        Payload::named("app.exe", b"app 2.0.0", Some("core")),
        Payload::named("block.dat", b"blocked", Some("core")),
    ];
    let v2 = compose(&next, FRONTEND, &blocked);
    fs::create_dir(install.join("block.dat")).expect("a directory in the way");

    let failed = v2.run(&state, "install", &[]);
    assert!(
        !failed.status.success(),
        "an install that cannot write a file must fail"
    );
    assert_eq!(
        ledger_version(&state, &app).as_deref(),
        Some("1.0.0"),
        "the committed version is the one that worked"
    );
    assert!(
        maintenance_v1.is_file(),
        "the previous maintenance copy is still there"
    );
    assert_eq!(
        registered(&app)["DisplayVersion"],
        AppsFeaturesValue::String("1.0.0".into())
    );

    // Clear the obstruction and the upgrade completes, moving the persisted copy
    // forward and retiring the old one.
    fs::remove_dir(install.join("block.dat")).expect("the obstruction is removable");
    v2.succeed(&state, "install", &[]);
    let maintenance_v2 = state.maintenance(&next);
    assert!(maintenance_v2.is_file());
    assert!(!maintenance_v1.exists());
    assert_eq!(ledger_version(&state, &app).as_deref(), Some("2.0.0"));
    assert_eq!(
        registered(&app)["DisplayVersion"],
        AppsFeaturesValue::String("2.0.0".into())
    );
    succeed(&maintenance_v2, &state, "uninstall", &[]);
}

#[test]
fn an_installer_whose_package_is_corrupt_installs_nothing() {
    // A package that fails its own integrity check is the one failure a runtime
    // must not work around. Refusing is correct; falling back to something else
    // would install a different application than the person asked for.
    let app = AppSpec::unique("corrupt");
    let state = State::new();
    let _cleanup = cleanup(&app, &state);
    let installer = project::compose_with_corrupt_package(&app, FRONTEND);
    assert!(
        zup_windows::EmbeddedBundle::open(installer.path()).is_err(),
        "the fixture has to be one the runtime refuses"
    );

    let refused = installer.run(&state, "install", &[]);
    assert!(!refused.status.success());
    assert!(!state.path().join("transactions").exists());
    assert!(!app.install_directory().exists());
    assert!(
        zup_windows::inspect_uninstall_registration(SelectedScope::User, &app.id)
            .expect("the registration is readable")
            .is_none(),
        "a refused install registers nothing"
    );
}
