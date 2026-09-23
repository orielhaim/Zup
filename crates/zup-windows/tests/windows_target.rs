//! Windows target resolution and read-only inspection tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zup_build::materialize;
use zup_core::SelectedScope;
use zup_exec::{FileOperationKind, MachineSnapshot, ObservedFileState, plan_execution};
use zup_manifest::{parse, parse_and_compile};
use zup_plan::{PlanRequest, plan};
use zup_platform::{KnownFolder, KnownFolderError, KnownFolderResolver};
use zup_windows::{
    FakeServiceReader, FakeShortcutReader, WindowsKnownFolderResolver, WindowsRegistryReader,
    WindowsTargetContext, inspect_files, inspect_target_with, resolve_target,
};

#[derive(Debug, Clone)]
struct FakeKnownFolders {
    root: PathBuf,
}

impl KnownFolderResolver for FakeKnownFolders {
    fn resolve(
        &self,
        folder: KnownFolder,
        scope: SelectedScope,
    ) -> Result<PathBuf, KnownFolderError> {
        let name = match folder {
            KnownFolder::ProgramFiles => "PF",
            KnownFolder::LocalAppData => "LocalAppData",
            KnownFolder::ProgramData => "PD",
            KnownFolder::StartMenu => "StartMenu",
            KnownFolder::Desktop => match scope {
                SelectedScope::User => "UserDesktop",
                SelectedScope::Machine => "PublicDesktop",
            },
            KnownFolder::Programs => match scope {
                SelectedScope::User => "UserPrograms",
                SelectedScope::Machine => "CommonPrograms",
            },
        };
        Ok(self.root.join(name))
    }
}

fn write(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn acme_manifest() -> &'static str {
    r#"
schema = 1

[app]
id = "com.acme.acme"
name = "Acme"
version = "1.4.0"

[source]
directory = "dist"

[install]
scope = "either"

[install.directory]
user = "${known.local_app_data}/Programs/${app.name}"
machine = "${known.program_files}/${app.name}"

[[components]]
id = "core"
name = "Application"
required = true

[[files]]
source = "Acme.exe"
destination = "${install}"
component = "core"

[[files]]
source = "acme-agent.exe"
destination = "${install}"
component = "core"

[[files]]
source = "bin/**/*"
destination = "${install}/bin"
component = "core"

[[shortcuts]]
location = "start-menu"
name = "Acme"
target = "${install}/Acme.exe"
component = "core"

[[path]]
value = "${install}/bin"
component = "core"

[[services]]
id = "acme-agent"
name = "acme-agent"
display_name = "Acme Agent"
binary = "${install}/acme-agent.exe"
start = "automatic"
component = "core"

[[protocols]]
scheme = "acme"
executable = "${install}/Acme.exe"
args = ["--url", "%1"]

[[file_types]]
extension = ".acme"
id = "Acme.Document"
description = "Acme Document"
executable = "${install}/Acme.exe"

[[actions]]
id = "setup"
type = "exec"
privilege = "machine"

[actions.apply]
command = "${install}/setup-helper.exe"
args = ["register"]
"#
}

fn pipeline(root: &Path, scope: SelectedScope) -> (TempDir, zup_platform::TargetPlan) {
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    write(&project.join("zup.toml"), b"");
    write(&project.join("dist/Acme.exe"), b"main-correct");
    write(&project.join("dist/acme-agent.exe"), b"agent-new");
    write(&project.join("dist/bin/acme.exe"), b"cli-new");
    write(&project.join("dist/setup-helper.exe"), b"helper");

    let source = acme_manifest();
    let manifest = parse(source).expect("parse");
    let installer = parse_and_compile(source).expect("compile");
    let build = materialize(&project.join("zup.toml"), &manifest, installer).expect("materialize");
    let install = plan(&build, &PlanRequest::new(scope)).expect("install plan");

    let context = WindowsTargetContext::with_resolver(
        FakeKnownFolders {
            root: root.to_path_buf(),
        },
        scope,
    );
    let target = resolve_target(&install, &context).expect("target");
    (dir, target)
}

fn temp_root() -> TempDir {
    TempDir::new().unwrap()
}

#[test]
fn resolve_target_leaves_no_templates_and_sets_lnk() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::User);

    let json = serde_json::to_string(&target).unwrap();
    assert!(!json.contains("${"), "unresolved vars: {json}");

    assert_eq!(target.shortcuts.len(), 1);
    let link = target.shortcuts[0].link_path.to_string();
    assert!(link.ends_with(".lnk"), "link: {link}");
    assert!(link.contains("UserPrograms"), "{link}");
}

#[test]
fn machine_shortcut_uses_common_programs() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::Machine);
    let link = target.shortcuts[0].link_path.to_string();
    assert!(link.contains("CommonPrograms"), "{link}");
    assert_eq!(target.path_entries[0].scope, SelectedScope::Machine);
    assert_eq!(target.protocols[0].scope, SelectedScope::Machine);
}

#[test]
fn inspect_files_absent_present_directory() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::User);
    let pf = root.path().join("LocalAppData/Programs/Acme");
    write(&pf.join("Acme.exe"), b"main-correct");
    write(&pf.join("acme-agent.exe"), b"agent-OLD");
    fs::create_dir_all(pf.join("bin/acme.exe")).unwrap();

    let files = inspect_files(&target).unwrap();
    assert_eq!(files.len(), 3);
    let mut by_name = BTreeMap::new();
    for f in &files {
        let name = f
            .path
            .as_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        by_name.insert(name, f.state.clone());
    }
    assert!(matches!(
        by_name["Acme.exe"],
        ObservedFileState::File { size: 12, .. }
    ));
    assert!(matches!(
        by_name["acme-agent.exe"],
        ObservedFileState::File { size: 9, .. }
    ));
    assert!(matches!(by_name["acme.exe"], ObservedFileState::NonFile));
}

#[test]
fn inspect_target_with_fakes_and_plan_execution() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::User);
    let pf = root.path().join("LocalAppData/Programs/Acme");
    write(&pf.join("Acme.exe"), b"main-correct");
    write(&pf.join("acme-agent.exe"), b"agent-OLD");
    // bin/acme.exe intentionally absent → Create
    write(&pf.join("setup-helper.exe"), b"helper");

    let services = FakeServiceReader::default();
    let shortcuts = FakeShortcutReader::default();
    let registry = WindowsRegistryReader;

    let snapshot = inspect_target_with(&target, &registry, &services, &shortcuts).unwrap();
    let plan = plan_execution(&target, &snapshot, None).unwrap();

    // An existing different file has no proven owner.
    let mut file_kinds = BTreeMap::new();
    for op in &plan.files {
        let name = op
            .destination
            .as_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        file_kinds.insert(name, op.kind);
    }
    assert_eq!(file_kinds["Acme.exe"], FileOperationKind::NoOp);
    assert_eq!(file_kinds["acme-agent.exe"], FileOperationKind::Conflict);
    assert_eq!(file_kinds["acme.exe"], FileOperationKind::Create);

    assert_eq!(
        plan.shortcuts[0].kind,
        zup_exec::ShortcutOperationKind::Create
    );
    assert_eq!(plan.external_actions.len(), 1);
    assert!(plan.external_actions[0].opaque);

    // Zero mutation: payload on disk unchanged.
    assert_eq!(fs::read(pf.join("acme-agent.exe")).unwrap(), b"agent-OLD");
}

#[test]
fn machine_snapshot_is_deterministic() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::User);
    let services = FakeServiceReader::default();
    let shortcuts = FakeShortcutReader::default();
    let registry = WindowsRegistryReader;

    let a = inspect_target_with(&target, &registry, &services, &shortcuts).unwrap();
    let b = inspect_target_with(&target, &registry, &services, &shortcuts).unwrap();
    assert_eq!(a, b);
    let _ = MachineSnapshot::default();
}

#[test]
#[cfg(windows)]
fn live_known_folders_include_programs() {
    let resolver = WindowsKnownFolderResolver;
    for scope in [SelectedScope::User, SelectedScope::Machine] {
        let path = resolver.resolve(KnownFolder::Programs, scope).unwrap();
        assert!(path.is_absolute());
        assert!(!path.as_os_str().is_empty());
        assert!(!path.to_string_lossy().contains("${"));
    }
}
