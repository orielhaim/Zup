use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use zup_bundle::BundleWriter;
use zup_core::{
    App, AppId, FileAssociation, FileAssociationId, FileExtension, Frontend, Install,
    InstallDirectory, InstallScope, Installer, Launcher, LauncherLocation, NonEmptyString,
    Protocol, ProtocolScheme, RelativePath, ResolvedFile, SelectedScope, TargetBuildPlan,
    TargetTriple, Template, hash_reader,
};
use zup_protocol::{SessionId, WireEnvelope};

use crate::error::{ExecError, IpcError};
use crate::machine::MachineRoots;
use crate::run::{LinuxAction, LinuxOutcome, LinuxRunRequest, run};
use crate::systemd::{SystemdManager, UnitChange, UnitInfo};

#[derive(Debug, Clone)]
pub struct MachineTestRoots {
    pub roots: MachineRoots,

    pub state: PathBuf,

    pub systemd: crate::machine::SystemdRoots,
}

impl MachineTestRoots {
    pub fn isolate_in(base: &Path) -> Self {
        let programs = base.join("opt");
        let state = base.join("var").join("lib").join("zup");
        let shared_data = base.join("var").join("opt");
        let units = base.join("units");
        for directory in [
            &programs,
            &shared_data,
            &units,
            state.parent().expect("a parent"),
        ] {
            std::fs::create_dir_all(directory).expect("an isolated root");
        }
        let roots = MachineRoots::new(programs, state.clone(), shared_data);
        let systemd = crate::machine::SystemdRoots::new(units);
        Self {
            roots,
            state,
            systemd,
        }
    }
}

pub fn run_machine_isolated(
    installer: &Path,
    test_roots: &MachineTestRoots,
    action: LinuxAction,
    install_dir_override: Option<PathBuf>,
) -> Result<LinuxOutcome, ExecError> {
    crate::elevate::run_machine_loopback_for_test(
        &LinuxRunRequest {
            installer: installer.to_path_buf(),
            scope: zup_core::SelectedScope::Machine,
            state_root: Some(test_roots.state.clone()),
            action,
            install_dir_override,
        },
        &test_roots.roots,
        &test_roots.systemd,
    )
}

pub fn serve_worker_isolated(
    stream: &mut UnixStream,
    roots: &MachineRoots,
    invoking_uid: u32,
    expected_client_pid: u32,
    session: SessionId,
    worker_exe: &Path,
) -> Result<String, crate::error::IpcError> {
    crate::worker::serve_session(
        stream,
        crate::worker::WorkerContext {
            roots: roots.clone(),
            systemd: crate::machine::SystemdRoots::production(),
            invoking_uid,
            expected_client_pid,
            session,
            worker_exe: worker_exe.to_path_buf(),
            carrier_pin: None,
        },
    )
}

pub fn drive_client_isolated(
    stream: &mut UnixStream,
    session: SessionId,
    intent: &zup_protocol::PrepareOperation,
    expected_digest: &str,
    expected_target: &zup_core::TargetTriple,
) -> Result<crate::run::LinuxOutcome, crate::error::ExecError> {
    crate::elevate::drive_client(stream, session, intent, expected_digest, expected_target)
}

pub fn plan_for_test(
    installer: &Path,
    test_roots: &MachineTestRoots,
    action: LinuxAction,
    install_dir_override: Option<PathBuf>,
) -> (
    zup_protocol::PrepareOperation,
    zup_transaction::TransactionPlan,
) {
    let request = crate::run::LinuxRunRequest {
        installer: installer.to_path_buf(),
        scope: zup_core::SelectedScope::Machine,
        state_root: Some(test_roots.state.clone()),
        action,
        install_dir_override,
    };
    let (intent, expected) = crate::elevate::plan_expected(
        &request,
        &test_roots.state.clone(),
        &test_roots.roots,
        &test_roots.systemd,
        rustix::process::geteuid().as_raw(),
    )
    .expect("the fixture plans");
    (intent, expected.plan)
}

pub fn validate_rendezvous_for_test(
    socket: &std::path::Path,
    invoking_uid: u32,
) -> Result<(), crate::error::IpcError> {
    crate::worker::validate_rendezvous(socket, invoking_uid)
}

pub fn run_machine_elevated_for_test(
    installer: &Path,
    state: &Path,
    action: LinuxAction,
    install_dir_override: Option<PathBuf>,
    launcher: &impl crate::pkexec::PkexecLauncher,
) -> Result<LinuxOutcome, ExecError> {
    crate::elevate::run_machine_elevated(
        &crate::run::LinuxRunRequest {
            installer: installer.to_path_buf(),
            scope: zup_core::SelectedScope::Machine,
            state_root: Some(state.to_path_buf()),
            action,
            install_dir_override,
        },
        launcher,
    )
}

pub fn send_envelope_on(
    stream: &mut UnixStream,
    envelope: &WireEnvelope,
) -> Result<(), crate::error::IpcError> {
    crate::socket::send_envelope(stream, envelope)
}

pub fn recv_envelope_on(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<WireEnvelope, crate::error::IpcError> {
    crate::socket::recv_envelope(stream, timeout)
}

#[derive(Debug, Clone)]
pub struct FakeSystemd {
    units: BTreeMap<String, FakeUnit>,

    pub version: String,

    pub fail_next: BTreeMap<String, String>,

    pub fail_always: BTreeMap<String, String>,

    pub lose_reply: BTreeMap<String, String>,

    pub extra_links: BTreeMap<String, Vec<String>>,
    pub reloads: usize,
}

impl Default for FakeSystemd {
    fn default() -> Self {
        Self {
            units: BTreeMap::new(),

            version: "259".into(),
            fail_next: BTreeMap::new(),
            fail_always: BTreeMap::new(),
            lose_reply: BTreeMap::new(),
            extra_links: BTreeMap::new(),
            reloads: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct FakeUnit {
    state: String,
    load_state: String,
    fragment_path: String,
}

impl FakeSystemd {
    pub fn seed(&mut self, unit: &str, state: &str, fragment_path: &str) {
        self.units.insert(
            unit.to_owned(),
            FakeUnit {
                state: state.to_owned(),
                load_state: if state == "masked" {
                    "masked".into()
                } else {
                    "loaded".into()
                },
                fragment_path: fragment_path.to_owned(),
            },
        );
    }

    pub fn seed_fragment(&mut self, unit: &str, fragment_path: &str) {
        let entry = self.units.entry(unit.to_owned()).or_insert(FakeUnit {
            state: "disabled".into(),
            load_state: "loaded".into(),
            fragment_path: String::new(),
        });
        entry.fragment_path = fragment_path.to_owned();
    }

    fn fail(&mut self, operation: &str, unit: &str) -> Option<IpcError> {
        if let Some(message) = self.fail_next.remove(operation) {
            return Some(IpcError::SystemdUnavailable(format!(
                "{operation} {unit}: {message}"
            )));
        }
        if let Some(message) = self.fail_always.get(operation) {
            return Some(IpcError::SystemdUnavailable(format!(
                "{operation} {unit}: {message}"
            )));
        }
        None
    }

    fn lost(&mut self, operation: &str) -> bool {
        self.lose_reply.remove(operation).is_some()
    }

    pub fn disable(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        if let Some(error) = self.fail("disable", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        entry.state = "disabled".into();
        let changes = vec![
            UnitChange::bound(
                "unlink",
                &format!("/etc/systemd/system/multi-user.target.wants/{unit}"),
                "",
            )
            .expect("static change bounds"),
        ];
        if self.lost("disable") {
            return Err(IpcError::SystemdUnavailable("disable reply lost".into()));
        }
        Ok(changes)
    }

    fn entry(&mut self, unit: &str) -> &mut FakeUnit {
        self.units.entry(unit.to_owned()).or_insert(FakeUnit {
            state: "disabled".into(),
            load_state: "loaded".into(),
            fragment_path: String::new(),
        })
    }
}

impl SystemdManager for FakeSystemd {
    fn reload(&mut self) -> Result<(), IpcError> {
        if let Some(error) = self.fail("reload", "") {
            return Err(error);
        }
        self.reloads += 1;
        if self.lost("reload") {
            return Err(IpcError::SystemdUnavailable("reload reply lost".into()));
        }
        Ok(())
    }

    fn version(&mut self) -> Result<String, IpcError> {
        if let Some(error) = self.fail("version", "") {
            return Err(error);
        }
        Ok(self.version.clone())
    }

    fn unit_file_state(&mut self, unit: &str) -> Result<String, IpcError> {
        if let Some(error) = self.fail("unit_file_state", unit) {
            return Err(error);
        }
        Ok(self
            .units
            .get(unit)
            .map(|entry| entry.state.clone())
            .unwrap_or_else(|| "disabled".to_owned()))
    }

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, IpcError> {
        if let Some(error) = self.fail("load_unit", unit) {
            return Err(error);
        }
        let entry = self.units.get(unit);
        Ok(UnitInfo {
            load_state: entry
                .map(|entry| entry.load_state.clone())
                .unwrap_or_else(|| "not-found".to_owned()),
            fragment_path: entry
                .map(|entry| entry.fragment_path.clone())
                .unwrap_or_default(),
            unit_file_state: entry
                .map(|entry| entry.state.clone())
                .unwrap_or_else(|| "disabled".to_owned()),
        })
    }

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        if let Some(error) = self.fail("enable", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        entry.state = "enabled".into();
        entry.load_state = "loaded".into();
        let changes = vec![
            UnitChange::bound(
                "symlink",
                &format!("/etc/systemd/system/multi-user.target.wants/{unit}"),
                &format!("/usr/local/lib/systemd/system/{unit}"),
            )
            .expect("static change bounds"),
        ];
        if self.lost("enable") {
            return Err(IpcError::SystemdUnavailable("enable reply lost".into()));
        }
        Ok(changes)
    }

    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        _canonical_source: &str,
    ) -> Result<Vec<UnitChange>, IpcError> {
        if let Some(error) = self.fail("remove_owned_enablement", unit) {
            return Err(error);
        }
        if let Some(extra) = self.extra_links.get(unit)
            && !extra.is_empty()
        {
            return Err(IpcError::SystemdAmbiguous {
                unit: unit.to_owned(),
                reason: format!(
                    "refusing owned-link removal: unrelated enablement exists: {}",
                    extra.join(", ")
                ),
            });
        }
        let entry = self.entry(unit);
        let removed = entry.state == "enabled" || entry.state == "enabled-runtime";
        entry.state = "disabled".into();
        let changes = removed
            .then(|| {
                UnitChange::bound(
                    "unlink",
                    &format!("/etc/systemd/system/multi-user.target.wants/{unit}"),
                    "",
                )
                .expect("static change bounds")
            })
            .into_iter()
            .collect();
        if self.lost("remove_owned_enablement") {
            return Err(IpcError::SystemdUnavailable(
                "owned-link removal reply lost".into(),
            ));
        }
        Ok(changes)
    }

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        if let Some(error) = self.fail("mask", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        entry.state = "masked".into();
        entry.load_state = "masked".into();
        let changes = vec![
            UnitChange::bound(
                "symlink",
                &format!("/etc/systemd/system/{unit}"),
                "/dev/null",
            )
            .expect("static change bounds"),
        ];
        if self.lost("mask") {
            return Err(IpcError::SystemdUnavailable("mask reply lost".into()));
        }
        Ok(changes)
    }

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        if let Some(error) = self.fail("unmask", unit) {
            return Err(error);
        }
        let entry = self.entry(unit);
        if entry.state == "masked" {
            entry.state = "disabled".into();
            entry.load_state = "loaded".into();
        }
        let changes = vec![
            UnitChange::bound("unlink", &format!("/etc/systemd/system/{unit}"), "")
                .expect("static change bounds"),
        ];
        if self.lost("unmask") {
            return Err(IpcError::SystemdUnavailable("unmask reply lost".into()));
        }
        Ok(changes)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SharedFakeSystemd(std::rc::Rc<std::cell::RefCell<FakeSystemd>>);

impl SharedFakeSystemd {
    pub fn borrow(&self) -> std::cell::Ref<'_, FakeSystemd> {
        self.0.borrow()
    }

    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, FakeSystemd> {
        self.0.borrow_mut()
    }
}

impl SystemdManager for SharedFakeSystemd {
    fn reload(&mut self) -> Result<(), IpcError> {
        self.0.borrow_mut().reload()
    }

    fn version(&mut self) -> Result<String, IpcError> {
        self.0.borrow_mut().version()
    }

    fn unit_file_state(&mut self, unit: &str) -> Result<String, IpcError> {
        self.0.borrow_mut().unit_file_state(unit)
    }

    fn load_unit(&mut self, unit: &str) -> Result<UnitInfo, IpcError> {
        self.0.borrow_mut().load_unit(unit)
    }

    fn enable(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        self.0.borrow_mut().enable(unit)
    }

    fn remove_owned_enablement(
        &mut self,
        unit: &str,
        canonical_source: &str,
    ) -> Result<Vec<UnitChange>, IpcError> {
        self.0
            .borrow_mut()
            .remove_owned_enablement(unit, canonical_source)
    }

    fn mask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        self.0.borrow_mut().mask(unit)
    }

    fn unmask(&mut self, unit: &str) -> Result<Vec<UnitChange>, IpcError> {
        self.0.borrow_mut().unmask(unit)
    }
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub struct IsolatedUser {
    _lock: MutexGuard<'static, ()>,
    prior: Vec<(String, Option<std::ffi::OsString>)>,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    pub state: PathBuf,
}

impl IsolatedUser {
    pub fn isolate() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().expect("a temp directory");
        let home = root.path().join("home");
        let mut prior = Vec::new();
        let dirs = [
            ("HOME", home.clone()),
            ("XDG_DATA_HOME", home.join(".local/share")),
            ("XDG_STATE_HOME", home.join(".local/state")),
            ("XDG_CONFIG_HOME", home.join(".config")),
            ("XDG_CACHE_HOME", home.join(".cache")),
            ("XDG_RUNTIME_DIR", root.path().join("run")),
        ];
        for (name, path) in dirs {
            std::fs::create_dir_all(&path).expect("an isolated directory");
            prior.push((name.to_owned(), std::env::var_os(name)));
            // SAFETY: the lock above serializes every environment mutation in

            unsafe { std::env::set_var(name, &path) };
        }
        let state = root.path().join("state-root");
        Self {
            _lock: lock,
            prior,
            root,
            state,
        }
    }

    pub fn programs(&self) -> PathBuf {
        let home = std::env::var_os("HOME").expect("HOME is isolated");
        PathBuf::from(home).join(".local/lib/zup/apps")
    }

    pub fn child_env(&self) -> Vec<(String, String)> {
        let home = std::env::var_os("HOME").expect("HOME is isolated");
        let home = PathBuf::from(home);
        let var =
            |name: &str, path: PathBuf| (name.to_owned(), path.to_string_lossy().into_owned());
        vec![
            var("HOME", home.clone()),
            var("XDG_DATA_HOME", home.join(".local/share")),
            var("XDG_STATE_HOME", home.join(".local/state")),
            var("XDG_CONFIG_HOME", home.join(".config")),
            var("XDG_CACHE_HOME", home.join(".cache")),
            var("XDG_RUNTIME_DIR", self.root.path().join("run")),
        ]
    }
}

impl Drop for IsolatedUser {
    fn drop(&mut self) {
        for (name, value) in self.prior.drain(..) {
            match value {
                // SAFETY: same serialization as the setup above.
                Some(previous) => unsafe { std::env::set_var(&name, previous) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }
}

pub struct FixtureFile {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

pub fn tool_script(version: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ -n \"$TOOL_LOG\" ]; then printf '%s\\n' \"$*\" >> \"$TOOL_LOG\"; fi\nif [ \"$1\" = \"--version\" ]; then echo \"tool {version}\"; exit 0; fi\nif [ \"$1\" = \"read-payload\" ]; then cat \"$(dirname \"$0\")/keep.dat\"; exit 0; fi\nif [ \"$1\" = \"--url\" ]; then echo \"opened $2\"; exit 0; fi\ncase \"$1\" in *.foo|*.bar) echo \"opened $1\"; exit 0;; esac\necho \"tool: unknown command $1\" >&2\nexit 1\n"
    )
    .into_bytes()
}

pub fn v1_files() -> Vec<FixtureFile> {
    vec![
        FixtureFile {
            name: "tool",
            bytes: tool_script("1.0.0"),
            executable: true,
        },
        FixtureFile {
            name: "keep.dat",
            bytes: b"keep-v1".to_vec(),
            executable: false,
        },
        FixtureFile {
            name: "removed-in-v2.dat",
            bytes: b"doomed".to_vec(),
            executable: false,
        },
    ]
}

pub fn v2_files() -> Vec<FixtureFile> {
    vec![
        FixtureFile {
            name: "tool",
            bytes: tool_script("1.1.0"),
            executable: true,
        },
        FixtureFile {
            name: "keep.dat",
            bytes: b"keep-v1".to_vec(),
            executable: false,
        },
        FixtureFile {
            name: "added-in-v2.dat",
            bytes: b"new".to_vec(),
            executable: false,
        },
    ]
}

pub fn package_bytes(scratch: &Path, version: &str, files: &[FixtureFile]) -> Vec<u8> {
    package_bytes_for(
        scratch,
        &TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target"),
        version,
        files,
    )
}

pub fn machine_package_bytes(
    scratch: &Path,
    version: &str,
    files: &[FixtureFile],
    allow_directory_override: bool,
) -> Vec<u8> {
    let target = TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target");
    let payload_dir = scratch.join("payload");
    std::fs::create_dir_all(&payload_dir).expect("a payload directory");
    let resolved = files
        .iter()
        .map(|file| {
            let source = payload_dir.join(file.name);
            std::fs::write(&source, &file.bytes).expect("a payload file");
            let (size, sha256) = hash_reader(file.bytes.as_slice()).expect("a payload hashes");
            ResolvedFile {
                source,
                source_relative: RelativePath::new(file.name).expect("a relative path"),
                destination: Template::parse(&format!("${{install}}/{}", file.name))
                    .expect("a destination"),
                size,
                sha256,
                component: None,
                condition: None,
                executable: file.executable,
            }
        })
        .collect::<Vec<_>>();
    let total_size = resolved.iter().map(|file| file.size).sum();
    let plan = TargetBuildPlan {
        installer: Installer {
            preset: None,
            app: App {
                id: AppId::new("com.example.tool").expect("an id"),
                name: NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse(version).expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: target.clone(),
            frontend: Frontend::Console,
            updates: None,
            install: Install {
                scope: InstallScope::Machine,
                directory: InstallDirectory {
                    user: None,
                    machine: Some(
                        Template::parse("${location.programs}/tool").expect("a directory"),
                    ),
                },
                allow_directory_override,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            component_groups: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
        },
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        total_size,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
        files: resolved,
        ui_assets: Vec::new(),
    };
    BundleWriter::encode(&plan, &[]).expect("the package encodes")
}

pub fn machine_fixture(
    scratch: &Path,
    file_name: &str,
    version: &str,
    files: &[FixtureFile],
) -> PathBuf {
    machine_fixture_override(scratch, file_name, version, files, false)
}

pub fn machine_fixture_override(
    scratch: &Path,
    file_name: &str,
    version: &str,
    files: &[FixtureFile],
    allow_directory_override: bool,
) -> PathBuf {
    let package = machine_package_bytes(scratch, version, files, allow_directory_override);
    let output = scratch.join(file_name);
    compose_installer(Path::new(inert_template()), &output, &package);
    output
}

pub fn machine_v1_files() -> Vec<FixtureFile> {
    vec![
        FixtureFile {
            name: "tool",
            bytes: tool_script("1.0.0"),
            executable: true,
        },
        FixtureFile {
            name: "keep.dat",
            bytes: b"keep-v1".to_vec(),
            executable: false,
        },
    ]
}

pub fn machine_v2_files() -> Vec<FixtureFile> {
    vec![
        FixtureFile {
            name: "tool",
            bytes: tool_script("2.0.0"),
            executable: true,
        },
        FixtureFile {
            name: "new.dat",
            bytes: b"new-v2".to_vec(),
            executable: false,
        },
    ]
}

pub fn machine_install_dir(roots: &crate::machine::MachineRoots) -> PathBuf {
    roots.programs.join("tool")
}

pub fn machine_maintenance_path(state: &Path, version: &str) -> PathBuf {
    zup_transaction::maintenance_runtime_path(
        state,
        &AppId::new("com.example.tool").expect("an id"),
        SelectedScope::Machine,
        &semver::Version::parse(version).expect("a version"),
        TargetTriple::parse("x86_64-unknown-linux-gnu")
            .expect("a Linux target")
            .executable_suffix(),
    )
}

pub struct FixtureIcon {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

pub fn menu_launcher(name: &str) -> Launcher {
    Launcher {
        location: LauncherLocation::Menu,
        name: NonEmptyString::new(name).expect("a name"),
        target: Template::parse("${location.programs}/tool/tool").expect("a target"),
        arguments: Vec::new(),
        working_directory: None,
        component: None,
        when: None,
    }
}

pub fn fixture_protocol(scheme: &str) -> Protocol {
    Protocol {
        scheme: ProtocolScheme::new(scheme).expect("a scheme"),
        executable: Template::parse("${location.programs}/tool/tool").expect("an executable"),
        args: vec!["--url".into(), "%1".into()],
        when: None,
    }
}

pub fn fixture_association(extension: &str, id: &str, description: &str) -> FileAssociation {
    FileAssociation {
        extension: FileExtension::new(extension).expect("an extension"),
        id: FileAssociationId::new(id).expect("an id"),
        description: Some(description.into()),
        executable: Template::parse("${location.programs}/tool/tool").expect("an executable"),
        when: None,
    }
}

pub fn v1_integration() -> (Vec<Launcher>, Vec<Protocol>, Vec<FileAssociation>) {
    (
        vec![menu_launcher("Tool")],
        vec![fixture_protocol("acme")],
        vec![fixture_association(".foo", "acme.foo", "Foo document")],
    )
}

pub fn v1_icons() -> Vec<FixtureIcon> {
    vec![
        FixtureIcon {
            name: "hicolor/48x48/apps/com.example.tool.png",
            bytes: b"icon-48-v1".to_vec(),
        },
        FixtureIcon {
            name: "hicolor/scalable/apps/com.example.tool.svg",
            bytes: b"<svg>icon-v1</svg>".to_vec(),
        },
    ]
}

pub fn v2_icons() -> Vec<FixtureIcon> {
    vec![
        FixtureIcon {
            name: "hicolor/64x64/apps/com.example.tool.png",
            bytes: b"icon-64-v2".to_vec(),
        },
        FixtureIcon {
            name: "hicolor/scalable/apps/com.example.tool.svg",
            bytes: b"<svg>icon-v2</svg>".to_vec(),
        },
    ]
}

pub fn package_bytes_for(
    scratch: &Path,
    target: &TargetTriple,
    version: &str,
    files: &[FixtureFile],
) -> Vec<u8> {
    package_bytes_full(scratch, target, version, files, &[], &[], &[], &[])
}

#[allow(clippy::too_many_arguments)]
pub fn package_bytes_full(
    scratch: &Path,
    target: &TargetTriple,
    version: &str,
    files: &[FixtureFile],
    launchers: &[Launcher],
    protocols: &[Protocol],
    associations: &[FileAssociation],
    icons: &[FixtureIcon],
) -> Vec<u8> {
    let target = target.clone();
    let payload_dir = scratch.join("payload");
    std::fs::create_dir_all(&payload_dir).expect("a payload directory");
    let resolved = files
        .iter()
        .map(|file| {
            let source = payload_dir.join(file.name);
            std::fs::write(&source, &file.bytes).expect("a payload file");
            let (size, sha256) = hash_reader(file.bytes.as_slice()).expect("a payload hashes");
            ResolvedFile {
                source,
                source_relative: RelativePath::new(file.name).expect("a relative path"),
                destination: Template::parse(&format!("${{location.programs}}/tool/{}", file.name))
                    .expect("a destination"),
                size,
                sha256,
                component: None,
                condition: None,
                executable: file.executable,
            }
        })
        .collect::<Vec<_>>();
    let mut resolved = resolved;
    for icon in icons {
        let source = payload_dir.join(icon.name.replace('/', "_"));
        std::fs::write(&source, &icon.bytes).expect("an icon file");
        let (size, sha256) = hash_reader(icon.bytes.as_slice()).expect("an icon hashes");
        resolved.push(ResolvedFile {
            source,
            source_relative: RelativePath::new(format!("__zup_icons__/{}", icon.name))
                .expect("an icon name"),
            destination: Template::parse(&format!("${{location.user_data}}/icons/{}", icon.name))
                .expect("an icon destination"),
            size,
            sha256,
            component: None,
            condition: None,
            executable: false,
        });
    }
    let total_size = resolved.iter().map(|file| file.size).sum();
    let plan = TargetBuildPlan {
        installer: Installer {
            preset: None,
            app: App {
                id: AppId::new("com.example.tool").expect("an id"),
                name: NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse(version).expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: target.clone(),
            frontend: Frontend::Console,
            updates: None,
            install: Install {
                scope: InstallScope::User,
                directory: InstallDirectory {
                    user: Some(Template::parse("${location.programs}/tool").expect("a directory")),
                    machine: None,
                },
                allow_directory_override: false,
            },
            prerequisites: Vec::new(),
            components: Vec::new(),
            component_groups: Vec::new(),
            plugins: Vec::new(),
            files: Vec::new(),
            launchers: launchers.to_vec(),
            path: Vec::new(),
            services: Vec::new(),
            protocols: protocols.to_vec(),
            file_associations: associations.to_vec(),
        },
        prerequisites: Vec::new(),
        plugins: Vec::new(),
        total_size,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
        files: resolved,
        ui_assets: Vec::new(),
    };
    BundleWriter::encode(&plan, &[]).expect("the package encodes")
}

pub fn compose_installer(template: &Path, output: &Path, package: &[u8]) {
    crate::carrier::compose(template, output, package).expect("compose");
}

pub fn inert_template() -> &'static str {
    "/bin/true"
}

pub fn compose_fixture(
    scratch: &Path,
    file_name: &str,
    version: &str,
    files: &[FixtureFile],
) -> PathBuf {
    let package = package_bytes(scratch, version, files);
    let output = scratch.join(file_name);
    compose_installer(Path::new(inert_template()), &output, &package);
    output
}

#[allow(clippy::too_many_arguments)]
pub fn compose_integration_fixture(
    scratch: &Path,
    file_name: &str,
    version: &str,
    files: &[FixtureFile],
    launchers: &[Launcher],
    protocols: &[Protocol],
    associations: &[FileAssociation],
    icons: &[FixtureIcon],
) -> PathBuf {
    let target = TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target");
    let package = package_bytes_full(
        scratch,
        &target,
        version,
        files,
        launchers,
        protocols,
        associations,
        icons,
    );
    let output = scratch.join(file_name);
    compose_installer(Path::new(inert_template()), &output, &package);
    output
}

pub fn data_home() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_DATA_HOME").expect("XDG_DATA_HOME is isolated"))
}

pub struct FakeTools {
    _lock: MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
    pub dir: PathBuf,
}

static TOOL_LOCK: Mutex<()> = Mutex::new(());

impl FakeTools {
    pub fn install() -> Self {
        let lock = TOOL_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("a tool directory");
        let dir = dir.keep();
        let prior = std::env::var_os("PATH");
        let mut paths = vec![dir.clone()];
        paths.extend(std::env::split_paths(&prior.clone().unwrap_or_default()));
        let joined = std::env::join_paths(paths).expect("PATH joins");
        // SAFETY: the lock above serializes every PATH mutation in this test

        unsafe { std::env::set_var("PATH", &joined) };
        Self {
            _lock: lock,
            prior,
            dir,
        }
    }

    pub fn tool(&self, name: &str, script: &str) {
        let path = self.dir.join(name);
        std::fs::write(&path, script).expect("a fake tool");
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod");
    }

    pub fn recording(&self, name: &str, sentinel: &Path) {
        self.tool(
            name,
            &format!(
                "#!/bin/sh\necho \"$*\" >> \"{}\"\nexit 0\n",
                sentinel.display()
            ),
        );
    }

    pub fn flaky(&self, name: &str, flag: &Path, sentinel: &Path) {
        self.tool(
            name,
            &format!(
                "#!/bin/sh\necho \"$*\" >> \"{}\"\nif [ -e \"{}\" ]; then exit 0; fi\nexit 1\n",
                sentinel.display(),
                flag.display()
            ),
        );
    }

    pub fn seal(&self) {
        // SAFETY: same serialization as the setup above.
        unsafe { std::env::set_var("PATH", &self.dir) };
    }
}

impl Drop for FakeTools {
    fn drop(&mut self) {
        match self.prior.take() {
            // SAFETY: same serialization as the setup above.
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

pub fn genuine_template(frontend: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("a test executable");

    let profile = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("a profile directory");
    let toolchain = profile.join("toolchain");
    let name = format!("zup-setup-{frontend}-x86_64-unknown-linux-gnu");
    let mut found = Vec::new();
    if let Ok(versions) = std::fs::read_dir(&toolchain) {
        for version in versions.flatten() {
            let candidate = version.path().join(&name);
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    assert!(
        !found.is_empty(),
        "no `{name}` in the staged toolchain under {}.\n\n  \
         The process tests need genuine templates. Build them once:\n    \
         cargo xtask toolchain build",
        toolchain.display()
    );
    found.sort();
    found.pop().expect("a template")
}

pub fn run_installer(installer: &Path, state: &Path, action: LinuxAction) -> LinuxOutcome {
    run(&LinuxRunRequest {
        installer: installer.to_path_buf(),
        scope: SelectedScope::User,
        state_root: Some(state.to_path_buf()),
        action,
        install_dir_override: None,
    })
    .expect("the run reaches a stable outcome")
}

pub fn run_installer_process(
    installer: &Path,
    user: &IsolatedUser,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(installer);
    command.args(args);
    command.arg("--state-root").arg(&user.state);

    command.env_clear();
    for (name, value) in user.child_env() {
        command.env(name, value);
    }

    command.env("PATH", "/usr/bin:/bin");
    command.output().expect("the installer process runs")
}

pub fn run_tool(tool: &Path, args: &[&str]) -> String {
    let output = Command::new(tool)
        .args(args)
        .output()
        .expect("the installed executable runs");
    assert!(
        output.status.success(),
        "exit {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 output")
}

pub fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777
}
