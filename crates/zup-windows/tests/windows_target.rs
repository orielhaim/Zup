//! Windows target resolution and read-only inspection tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zup_build::materialize;
use zup_core::{INSTALL_LOCATIONS, InstallLocation, SelectedScope, TargetTriple};
use zup_exec::{FileOperationKind, ObservedFileState, plan_execution};
use zup_manifest::{TargetOverrides, compile, parse, select_targets};
use zup_plan::{InstallPlan, PlanRequest, plan};
use zup_platform::{InstallLocationError, InstallLocationResolver, TargetPath};
use zup_windows::{
    FakeServiceReader, FakeShortcutReader, TargetResolveError, WindowsInstallLocationResolver,
    WindowsRegistryReader, WindowsTargetContext, inspect_files, inspect_target_with,
    resolve_target,
};

#[derive(Debug, Clone)]
struct FakeInstallLocations {
    root: PathBuf,
}

impl InstallLocationResolver for FakeInstallLocations {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let name = match location {
            InstallLocation::Programs => "PF",
            InstallLocation::UserData => "LocalAppData",
            InstallLocation::SharedData => "PD",
            InstallLocation::Menu => match scope {
                SelectedScope::User => "UserStartMenu",
                SelectedScope::Machine => "CommonStartMenu",
            },
            InstallLocation::Desktop => match scope {
                SelectedScope::User => "UserDesktop",
                SelectedScope::Machine => "PublicDesktop",
            },
        };
        let path = self.root.join(name);
        TargetPath::new(target, path.to_string_lossy().as_ref()).map_err(|source| {
            InstallLocationError::ResolutionFailed {
                location,
                scope,
                source: source.into(),
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedInstallLocations;

impl InstallLocationResolver for FixedInstallLocations {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let path = match location {
            InstallLocation::Programs => r"C:\Program Files",
            InstallLocation::UserData => r"C:\Users\Test\AppData\Local",
            InstallLocation::SharedData => r"C:\ProgramData",
            InstallLocation::Menu => match scope {
                SelectedScope::User => r"C:\Users\Test\Start Menu",
                SelectedScope::Machine => r"C:\ProgramData\Start Menu",
            },
            InstallLocation::Desktop => match scope {
                SelectedScope::User => r"C:\Users\Test\Desktop",
                SelectedScope::Machine => r"C:\Users\Public\Desktop",
            },
        };
        TargetPath::new(target, path).map_err(|source| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: source.into(),
        })
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

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "either"

[install.directory]
user = "${location.user_data}/Programs/${app.name}"
machine = "${location.programs}/${app.name}"

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

[[launchers]]
location = "menu"
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

[[file_associations]]
extension = ".acme"
id = "Acme.Document"
description = "Acme Document"
executable = "${install}/Acme.exe"
"#
}

fn semantic_plan(source: &str, files: &[(&str, &[u8])]) -> (TempDir, InstallPlan) {
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    write(&project.join("zup.toml"), b"");
    for (relative, contents) in files {
        write(&project.join(relative), contents);
    }
    let manifest = parse(source).expect("parse");
    let overrides = TargetOverrides::default();
    let config = select_targets(&manifest, &["default"], &overrides)
        .expect("target")
        .into_iter()
        .next()
        .expect("selected target");
    let installer = compile(&manifest, &config, &overrides).expect("compile");
    let build = materialize(
        &project.join("zup.toml"),
        &manifest,
        vec![(config.clone(), installer)],
    )
    .expect("materialize");
    let install = plan(
        &build,
        &PlanRequest::new(config.target.clone(), SelectedScope::User),
    )
    .expect("semantic plan");
    (dir, install)
}

fn resolve_fixed(
    install: &InstallPlan,
) -> Result<zup_platform::TargetPlan, zup_windows::TargetResolveError> {
    resolve_target(
        install,
        &WindowsTargetContext::with_resolver(FixedInstallLocations, SelectedScope::User),
    )
}

fn pipeline(root: &Path, scope: SelectedScope) -> (TempDir, zup_platform::TargetPlan) {
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    write(&project.join("zup.toml"), b"");
    write(&project.join("dist/Acme.exe"), b"main-correct");
    write(&project.join("dist/acme-agent.exe"), b"agent-new");
    write(&project.join("dist/bin/acme.exe"), b"cli-new");

    let source = acme_manifest();
    let manifest = parse(source).expect("parse");
    let overrides = TargetOverrides::default();
    let config = select_targets(&manifest, &["default"], &overrides)
        .expect("target")
        .into_iter()
        .next()
        .expect("selected target");
    let installer = compile(&manifest, &config, &overrides).expect("compile");
    let build = materialize(
        &project.join("zup.toml"),
        &manifest,
        vec![(config.clone(), installer)],
    )
    .expect("materialize");
    let install =
        plan(&build, &PlanRequest::new(config.target.clone(), scope)).expect("install plan");

    let context = WindowsTargetContext::with_resolver(
        FakeInstallLocations {
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
fn resolved_template_components_are_validated_after_semantic_planning() {
    let files = [
        ("dist/Acme.exe", b"main-correct".as_slice()),
        ("dist/acme-agent.exe", b"agent-new".as_slice()),
        ("dist/bin/acme.exe", b"cli-new".as_slice()),
    ];
    let cases = [
        (
            "install directory",
            acme_manifest().replace("name = \"Acme\"", "name = \"CON.\""),
        ),
        (
            "file destination",
            acme_manifest().replacen(
                "destination = \"${install}\"",
                "destination = \"${install}/file.\"",
                1,
            ),
        ),
        (
            "launcher target",
            acme_manifest().replace(
                "target = \"${install}/Acme.exe\"",
                "target = \"${install}/CON\"",
            ),
        ),
        (
            "launcher working directory",
            acme_manifest().replace(
                "target = \"${install}/Acme.exe\"",
                "target = \"${install}/Acme.exe\"\nworking_directory = \"${install}/bad.\"",
            ),
        ),
        (
            "PATH entry",
            acme_manifest().replace("value = \"${install}/bin\"", "value = \"${install}/CON\""),
        ),
        (
            "service binary",
            acme_manifest().replace(
                "binary = \"${install}/acme-agent.exe\"",
                "binary = \"${install}/CON\"",
            ),
        ),
        (
            "protocol executable",
            acme_manifest().replace(
                "executable = \"${install}/Acme.exe\"",
                "executable = \"${install}/CON\"",
            ),
        ),
        (
            "file association executable",
            acme_manifest()
                .rsplit_once("executable = \"${install}/Acme.exe\"")
                .map_or_else(
                    || acme_manifest().to_owned(),
                    |(prefix, _)| format!("{prefix}executable = \"${{install}}/CON\""),
                ),
        ),
    ];

    for (kind, source) in cases {
        let (_dir, install) = semantic_plan(&source, &files);
        let error = resolve_fixed(&install).expect_err(kind);
        assert!(
            matches!(
                error,
                TargetResolveError::InvalidTargetPath { kind: ref actual, .. }
                    if actual == kind
            ),
            "{kind}: {error}"
        );
    }
}

#[test]
fn case_only_file_destinations_collide_after_target_lowering() {
    let source = format!(
        "{}\n[[files]]\nsource = \"x/Foo.dll\"\ndestination = \"${{install}}/Payload.dll\"\n\n[[files]]\nsource = \"y/foo.dll\"\ndestination = \"${{install}}/payload.dll\"\n",
        acme_manifest()
    );
    let (_dir, install) = semantic_plan(
        &source,
        &[
            ("dist/Acme.exe", b"main-correct".as_slice()),
            ("dist/acme-agent.exe", b"agent-new".as_slice()),
            ("dist/bin/acme.exe", b"cli-new".as_slice()),
            ("dist/x/Foo.dll", b"one".as_slice()),
            ("dist/y/foo.dll", b"two".as_slice()),
        ],
    );
    let error = resolve_fixed(&install).expect_err("case-only collision");
    assert!(
        matches!(
            error,
            TargetResolveError::TargetCollision {
                ref kind, ..
            } if kind == "file destination"
        ),
        "{error}"
    );
}

#[test]
fn target_resource_identities_collide_case_insensitively() {
    let base = acme_manifest();
    let prefix = base.split("[[components]]").next().unwrap();
    let cases = [
        (
            "PATH entry",
            r#"
[[path]]
value = "${install}/Bin"

[[path]]
value = "${install}/bin"
"#,
        ),
        (
            "service id",
            r#"
[[services]]
id = "Agent"
name = "Agent"
binary = "${install}/Agent.exe"
start = "manual"

[[services]]
id = "agent"
name = "agent"
binary = "${install}/Agent.exe"
start = "manual"
"#,
        ),
        (
            "protocol scheme",
            r#"
[[protocols]]
scheme = "Acme"
executable = "${install}/Acme.exe"

[[protocols]]
scheme = "acme"
executable = "${install}/Acme.exe"
"#,
        ),
        (
            "file association id",
            r#"
[[file_associations]]
extension = ".one"
id = "Acme.One"
executable = "${install}/Acme.exe"

[[file_associations]]
extension = ".two"
id = "acme.one"
executable = "${install}/Acme.exe"
"#,
        ),
        (
            "launcher link path",
            r#"
[[launchers]]
location = "desktop"
name = "Launch"
target = "${install}/Acme.exe"

[[launchers]]
location = "desktop"
name = "launch"
target = "${install}/Acme.exe"
"#,
        ),
    ];

    for (kind, body) in cases {
        let source = format!("{prefix}\n{body}");
        let (_dir, install) = semantic_plan(&source, &[("dist/app.txt", b"app".as_slice())]);
        let error = resolve_fixed(&install).expect_err(kind);
        assert!(
            matches!(
                error,
                TargetResolveError::TargetCollision { kind: ref actual, .. }
                    if actual == kind
            ),
            "{kind}: {error}"
        );
    }
}

#[test]
fn resolve_target_leaves_no_templates_and_sets_lnk() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::User);

    let json = serde_json::to_string(&target).unwrap();
    assert!(!json.contains("${"), "unresolved vars: {json}");

    assert_eq!(target.launchers.len(), 1);
    let link = target.launchers[0].launcher_path.to_string();
    assert!(link.ends_with(".lnk"), "link: {link}");
    assert!(link.contains("UserStartMenu\\Programs"), "{link}");
}

#[test]
fn machine_shortcut_uses_common_programs() {
    let root = temp_root();
    let (_dir, target) = pipeline(root.path(), SelectedScope::Machine);
    let link = target.launchers[0].launcher_path.to_string();
    assert!(link.contains("CommonStartMenu\\Programs"), "{link}");
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
        let name = f.path.file_name().unwrap().to_owned();
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

    let services = FakeServiceReader::default();
    let shortcuts = FakeShortcutReader::default();
    let registry = WindowsRegistryReader;

    let snapshot = inspect_target_with(&target, &registry, &services, &shortcuts).unwrap();
    let plan = plan_execution(&target, &snapshot, None).unwrap();

    // An existing different file has no proven owner.
    let mut file_kinds = BTreeMap::new();
    for op in &plan.files {
        let name = op.destination.file_name().unwrap().to_owned();
        file_kinds.insert(name, op.kind);
    }
    assert_eq!(file_kinds["Acme.exe"], FileOperationKind::NoOp);
    assert_eq!(file_kinds["acme-agent.exe"], FileOperationKind::Conflict);
    assert_eq!(file_kinds["acme.exe"], FileOperationKind::Create);

    assert_eq!(
        plan.launchers[0].kind,
        zup_exec::LauncherOperationKind::Create
    );

    // Zero mutation: payload on disk unchanged.
    assert_eq!(fs::read(pf.join("acme-agent.exe")).unwrap(), b"agent-OLD");
}

#[test]
#[cfg(windows)]
fn live_semantic_locations_resolve_to_absolute_lexical_paths() {
    let resolver = WindowsInstallLocationResolver;
    let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
    for location in INSTALL_LOCATIONS {
        for scope in [SelectedScope::User, SelectedScope::Machine] {
            let path = resolver.resolve(location, scope, &target).unwrap();
            assert_eq!(path.target(), &target, "location: {location}");
            assert!(!path.as_str().is_empty());
            assert!(!path.as_str().contains("${"));
        }
    }
}

/// The resolver and the state root are the same question asked two ways, so they
/// are answered from the same place: a user install's state lives in the user
/// data directory that `${location.user_data}` resolves to, whatever the machine
/// happens to call that directory.
#[test]
fn the_user_state_root_and_the_user_data_location_are_one_directory() {
    let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
    let user_data = WindowsInstallLocationResolver
        .resolve(InstallLocation::UserData, SelectedScope::User, &target)
        .unwrap();
    let state = zup_windows::default_state_root(SelectedScope::User).unwrap();
    // A state root is canonicalized, because the ledger treats two spellings of
    // one directory as two identities, so the location is compared the same way.
    let expected = PathBuf::from(user_data.as_str())
        .join("zup")
        .canonicalize()
        .unwrap();
    assert_eq!(
        expected, state,
        "zup's own state sits in the same user data root an installed application uses"
    );
}

mod transaction_fingerprint {
    use std::path::Path;

    use tempfile::TempDir;
    use zup_build::{BuildPlan, TargetBuildPlan};
    use zup_core::{
        App, AppId, Component, ComponentId, Frontend, Install, InstallDirectory, InstallScope,
        Installer, NonEmptyString, PluginBinding, PluginId, SelectedScope, Sha256Digest,
        TargetTriple, Template,
    };
    use zup_exec::LifecycleAction;
    use zup_plan::{
        CancellationQuery, NeverCancelled, PlanRequest, PluginExecutor, PluginFailure,
        PluginPlanningContext, PluginResource, PluginResourceProposal, plan_with_plugins,
    };
    use zup_windows::{WindowsTargetContext, plan_target_lifecycle, resolve_target};

    use super::FakeInstallLocations;

    struct GuestExecutor {
        target: TargetTriple,
        resources: Vec<PluginResource>,
    }

    impl PluginExecutor for GuestExecutor {
        fn target(&self) -> &TargetTriple {
            &self.target
        }

        fn plan(
            &mut self,
            _binding: &PluginBinding,
            _context: &PluginPlanningContext,
            _cancellation: &dyn CancellationQuery,
        ) -> Result<PluginResourceProposal, PluginFailure> {
            Ok(PluginResourceProposal::new(self.resources.clone()))
        }
    }

    fn build_plan() -> BuildPlan {
        BuildPlan {
            targets: vec![TargetBuildPlan {
                installer: Installer {
                    target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
                    frontend: Frontend::Gui,
                    preset: None,
                    app: App {
                        id: AppId::new("com.example.fingerprint").unwrap(),
                        name: NonEmptyString::new("Fingerprint").unwrap(),
                        version: "1.0.0".parse().unwrap(),
                        publisher: None,
                        main: None,
                        description: None,
                    },
                    updates: None,
                    prerequisites: Vec::new(),
                    install: Install {
                        scope: InstallScope::User,
                        directory: InstallDirectory {
                            user: Some(
                                Template::parse("${location.user_data}/Fingerprint").unwrap(),
                            ),
                            machine: None,
                        },
                        allow_directory_override: false,
                    },
                    components: vec![Component {
                        id: ComponentId::new("core").unwrap(),
                        name: NonEmptyString::new("Core").unwrap(),
                        description: None,
                        required: true,
                        default: true,
                        requires: Vec::new(),
                    }],
                    plugins: vec![PluginBinding {
                        id: PluginId::new("guest").unwrap(),
                        component: None,
                        when: None,
                    }],
                    files: Vec::new(),
                    launchers: Vec::new(),
                    path: Vec::new(),
                    services: Vec::new(),
                    protocols: Vec::new(),
                    file_associations: Vec::new(),
                },
                prerequisites: Vec::new(),
                plugins: Vec::new(),
                files: Vec::new(),
                ui_assets: Vec::new(),
                total_size: 0,
                prerequisite_size: 0,
            }],
        }
    }

    fn fingerprint(root: &Path, state: &Path, resources: Vec<PluginResource>) -> Sha256Digest {
        let mut executor = GuestExecutor {
            target: TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
            resources,
        };
        let planned = plan_with_plugins(
            &build_plan(),
            &PlanRequest::new(
                TargetTriple::parse("x86_64-pc-windows-msvc").unwrap(),
                SelectedScope::User,
            ),
            &mut executor,
            &NeverCancelled,
        )
        .unwrap();
        let target = resolve_target(
            &planned.plan,
            &WindowsTargetContext::with_resolver(
                FakeInstallLocations {
                    root: root.to_path_buf(),
                },
                SelectedScope::User,
            ),
        )
        .unwrap();
        let execution = plan_target_lifecycle(
            LifecycleAction::Install,
            &target.app.id,
            SelectedScope::User,
            Some(&target),
            state,
        )
        .unwrap();
        execution.fingerprint()
    }

    #[test]
    fn reversed_guest_resources_have_one_fingerprint_but_content_changes_it() {
        let root = TempDir::new().unwrap();
        let state = root.path().join("state");
        let first = vec![
            PluginResource::GeneratedFile {
                destination: "${install}/a.txt".to_owned(),
                contents: b"a".to_vec(),
            },
            PluginResource::GeneratedFile {
                destination: "${install}/b.txt".to_owned(),
                contents: b"b".to_vec(),
            },
        ];
        let mut reversed = first.clone();
        reversed.reverse();
        let first_fingerprint = fingerprint(root.path(), &state, first);
        let reversed_fingerprint = fingerprint(root.path(), &state, reversed);
        assert_eq!(first_fingerprint, reversed_fingerprint);

        let changed = fingerprint(
            root.path(),
            &state,
            vec![
                PluginResource::GeneratedFile {
                    destination: "${install}/a.txt".to_owned(),
                    contents: b"changed".to_vec(),
                },
                PluginResource::GeneratedFile {
                    destination: "${install}/b.txt".to_owned(),
                    contents: b"b".to_vec(),
                },
            ],
        );
        assert_ne!(first_fingerprint, changed);
    }
}
