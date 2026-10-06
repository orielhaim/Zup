//! Building a real, installable `Setup.exe` for a runtime test.
//!
//! The runtime's tests need the file an end user double-clicks, and they have to
//! build it themselves. A test that depended on the developer CLI to produce its
//! fixture would be a test of the compiler, in a package that is supposed to have
//! nothing to do with the compiler - and a build-plane dev-dependency here would
//! put the very boundary this package exists to hold back into its own graph.
//!
//! So the plan is written out literally and embedded into a real runtime template
//! from the staged toolchain. That is the same package an `Acme-Setup.exe` carries,
//! produced without a manifest and without a source tree - which is exactly the
//! position the runtime is in when it runs on a user's machine.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zup_bundle::BundleWriter;
use zup_core::{
    App, AppId, Component, ComponentId, Frontend, Install, InstallDirectory, InstallScope,
    NonEmptyString, RelativePath, ResolvedFile, ResolvedPlugin, TargetBuildPlan, TargetTriple,
    Template, hash_reader,
};

/// One file the installer will place.
pub struct Payload {
    /// The name it travels under, and the file name it lands with.
    pub name: String,
    pub bytes: Vec<u8>,
    /// The component that owns it, if any.
    pub component: Option<&'static str>,
}

impl Payload {
    /// A file in the install directory's root, owned by `component`.
    ///
    /// The destination is the file's own path, not the directory: a plan's
    /// destination is resolved by Windows lowering, and `${install}` on its own
    /// describes a directory, which is the path the install directory itself
    /// already claims.
    pub fn named(name: &str, bytes: &[u8], component: Option<&'static str>) -> Self {
        Self {
            name: name.to_owned(),
            bytes: bytes.to_vec(),
            component,
        }
    }

    /// Where this file is written.
    fn destination(&self) -> String {
        format!("${{install}}/{}", self.name)
    }
}

/// What an application under test is called and where it goes.
#[derive(Clone)]
pub struct AppSpec {
    pub id: String,
    pub name: String,
    pub version: String,
    /// The install directory, as a template over zup's location variables.
    pub install_directory: String,
}

impl AppSpec {
    /// An application with a fresh identifier, so two tests never contend for one
    /// installation directory or one Apps & Features key.
    pub fn unique(label: &str) -> Self {
        let token = uuid::Uuid::now_v7().simple().to_string();
        Self {
            id: format!("com.zup.test-{label}-{token}"),
            name: format!("Zup Test {label}"),
            version: "1.0.0".to_owned(),
            install_directory: format!("${{location.user_data}}/Programs/ZupTest{label}-{token}"),
        }
    }

    /// The same application at another version, for an upgrade.
    pub fn at_version(&self, version: &str) -> Self {
        Self {
            version: version.to_owned(),
            ..self.clone()
        }
    }

    /// The install directory this application resolves to on this machine.
    pub fn install_directory(&self) -> PathBuf {
        zup_windows::user_data()
            .expect("a user data directory")
            .join("Programs")
            .join(
                self.install_directory
                    .rsplit('/')
                    .next()
                    .expect("a file name"),
            )
    }
}

/// A state root a test drives the runtime against, shared with nothing.
///
/// The directory is removed by [`cleanup`], which every test that installs
/// something holds; keeping it here would delete it out from under a test that
/// is still reading it.
pub struct State {
    root: PathBuf,
}

impl State {
    pub fn new() -> Self {
        Self::with_root(TempDir::new().expect("a state root").keep())
    }

    /// A state root at a path the caller chose.
    ///
    /// For a test that has to reach the same directory the runtime was given -
    /// to rewrite the record it wrote, for instance.
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Where the persisted maintenance copy for one application and version lives.
    ///
    /// Built one segment at a time, because a path containing a forward slash is
    /// the same path to the filesystem and a different string to everything that
    /// compares paths - which is exactly what a registry assertion does.
    pub fn maintenance(&self, app: &AppSpec) -> PathBuf {
        self.root
            .join("maintenance")
            .join(&app.id)
            .join("user")
            .join(&app.version)
            .join("maintenance.exe")
    }
}

/// Remove what a test put on this machine, whether or not the test passed.
///
/// An install directory, an Apps & Features key, and a state root are all real
/// user-visible things. A failing test that leaves them behind makes the next run
/// fail for a reason that has nothing to do with the code, which is how a real
/// defect gets reported as a flaky one.
pub struct Cleanup {
    app_id: String,
    install: PathBuf,
    state_root: PathBuf,
}

/// Remove everything `app` could have left behind in `state`.
pub fn cleanup(app: &AppSpec, state: &State) -> Cleanup {
    Cleanup {
        app_id: app.id.clone(),
        install: app.install_directory(),
        state_root: state.path().to_path_buf(),
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.install);
        let key = format!(
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{}",
            self.app_id
        );
        let _ = windows_registry::CURRENT_USER.remove_tree(key);
        let _ = std::fs::remove_dir_all(&self.state_root);
    }
}

/// A runnable installer for one application.
pub struct Setup {
    /// The scratch directory the file lives in.
    ///
    /// Held because an installer is read by the process that runs it, and a test
    /// that deleted it first would be testing `std::fs` rather than the runtime.
    scratch: TempDir,
    path: PathBuf,
}

impl Setup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run a lifecycle verb on this installer against a private state root.
    pub fn run(&self, state: &State, verb: &str, extra: &[&str]) -> std::process::Output {
        run(&self.path, state, verb, extra)
    }

    /// Run a lifecycle verb and require it to succeed.
    pub fn succeed(&self, state: &State, verb: &str, extra: &[&str]) -> std::process::Output {
        succeed(&self.path, state, verb, extra)
    }
}

/// Run a lifecycle verb on any executable, against a private state root.
///
/// The maintenance copy an installation persists is an executable this module
/// never composed, so the verb runner is a free function rather than a method: a
/// test drives the setup file and the persisted copy with the same code.
pub fn run(exe: &Path, state: &State, verb: &str, extra: &[&str]) -> std::process::Output {
    std::process::Command::new(exe)
        .arg(verb)
        .args(["--scope", "user", "--state-root"])
        .arg(state.path())
        .args(extra)
        .output()
        .unwrap_or_else(|error| panic!("run `{} {verb}`: {error}", exe.display()))
}

/// Run a lifecycle verb and require it to succeed.
///
/// Both streams in the failure, because a runtime told to report in JSON puts
/// its error on stdout and a test that only showed stderr would report a blank
/// reason for a real failure.
pub fn succeed(exe: &Path, state: &State, verb: &str, extra: &[&str]) -> std::process::Output {
    let output = run(exe, state, verb, extra);
    assert!(
        output.status.success(),
        "{verb} on {}:\n{}\n{}",
        exe.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A plugin the installer will run at install time.
///
/// The source is a build-machine path recorded in the plan. The runtime never
/// reads it - the compiled artifact is what ships - which is the property the
/// generated-file lifecycle test exists to prove.
pub struct PluginSpec {
    pub id: String,
    /// Where the plugin source was at build time. Recorded, never read.
    pub source: PathBuf,
    /// The name it travels under in the plan.
    pub source_relative: String,
}

impl PluginSpec {
    pub fn new(id: &str, source: &Path) -> Self {
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .expect("a source file name");
        Self {
            id: id.to_owned(),
            source: source.to_path_buf(),
            source_relative: name,
        }
    }
}

/// Compose the installer an end user would download.
///
/// `frontend` selects the runtime image the package is embedded into, so the
/// presentation a test drives is the presentation whose binary it named.
pub fn compose(app: &AppSpec, frontend: Frontend, payload: &[Payload]) -> Setup {
    compose_with(app, frontend, payload, &[], &[])
}

/// The window an installer will present, and what the application configured
/// for it.
pub struct PresetSpec {
    /// The preset executable, exactly as a `.zupui` target binary would carry it.
    pub executable: Vec<u8>,
    /// What the application wrote in `[ui.settings]`.
    pub settings: serde_json::Value,
    /// The application-provided assets, by the name its settings used.
    pub assets: Vec<(String, Vec<u8>)>,
    /// The capabilities the preset cannot present without.
    pub required_capabilities: Vec<zup_preset_protocol::Capability>,
}

/// Compose an installer that carries a real preset and real preset assets.
///
/// The preset travels as the selected target binary and the assets travel in the
/// package's content store, both of them resolved from files on disk the way a
/// build resolves them - so the installer this produces is the same shape a
/// `zup build` produces, and a test that runs it is running the real pipeline.
pub fn compose_with_preset(app: &AppSpec, payload: &[Payload], preset: &PresetSpec) -> Setup {
    let scratch = TempDir::new().expect("a build scratch directory");
    let mut plan = plan(app, Frontend::Gui, payload, &[], scratch.path());
    let mut assets = Vec::new();
    let mut records = Vec::new();
    for (name, bytes) in &preset.assets {
        let source = scratch.path().join(name.replace('/', "_"));
        std::fs::write(&source, bytes).expect("an asset file");
        let (size, sha256) = hash_reader(bytes.as_slice()).expect("an asset hashes");
        records.push(zup_core::PresetAsset {
            name: zup_core::NonEmptyString::new(name.as_str()).expect("a name"),
            size,
            sha256,
        });
        assets.push(zup_core::ResolvedAsset {
            name: zup_core::NonEmptyString::new(name.as_str()).expect("a name"),
            source: Some(source),
            source_relative: Some(RelativePath::new(name.as_str()).expect("a portable asset path")),
            size,
            sha256,
        });
    }
    plan.ui_assets = assets;
    plan.installer.preset = Some(zup_core::PresetRuntime {
        name: zup_core::NonEmptyString::new(format!("{}-preset", app.name)).expect("a name"),
        version: semver::Version::parse(&app.version).expect("a version"),
        protocol: zup_preset_protocol::PRESET_PROTOCOL_VERSION,
        required_capabilities: preset
            .required_capabilities
            .iter()
            .map(|capability| capability.to_string())
            .collect(),
        settings: preset.settings.clone(),
        assets: records,
    });

    let package = BundleWriter::encode(&plan, &[]).expect("the package encodes");
    let package_path = scratch.path().join("package.zupbundle");
    std::fs::write(&package_path, package).expect("the package is written");

    let path = scratch.path().join("Setup.exe");
    zup_windows::embed_bundle_file(
        &template(Frontend::Gui),
        &path,
        &package_path,
        Some(&preset.executable),
    )
    .expect("the package and the preset embed");
    assert!(
        zup_windows::EmbeddedBundle::open(&path).is_ok(),
        "the composed installer at {} reads back as one",
        path.display()
    );
    Setup { scratch, path }
}

/// Compose an installer that carries compiled plugin artifacts alongside its
/// payload.
pub fn compose_with(
    app: &AppSpec,
    frontend: Frontend,
    payload: &[Payload],
    plugins: &[PluginSpec],
    artifacts: &[zup_bundle::CompiledPluginArtifact],
) -> Setup {
    let scratch = TempDir::new().expect("a build scratch directory");
    let plan = plan(app, frontend, payload, plugins, scratch.path());
    let package = BundleWriter::encode(&plan, artifacts).expect("the package encodes");
    let package_path = scratch.path().join("package.zupbundle");
    std::fs::write(&package_path, package).expect("the package is written");

    let path = scratch.path().join("Setup.exe");
    let template = template(frontend);
    zup_windows::embed_bundle_file(&template, &path, &package_path, None)
        .expect("the package embeds");
    assert!(
        zup_windows::EmbeddedBundle::open(&path).is_ok(),
        "the composed installer at {} reads back as one",
        path.display()
    );
    Setup { scratch, path }
}

/// Compose an installer whose embedded package does not parse.
///
/// The corruption is one byte inside the package's data region, not the
/// executable's header: a launcher whose package will not parse is the failure a
/// user can actually meet - a truncated download, a partial copy, a bad sector -
/// and a fixture that only broke the PE would be testing something else.
pub fn compose_with_corrupt_package(app: &AppSpec, frontend: Frontend) -> Setup {
    let scratch = TempDir::new().expect("a build scratch directory");
    // One payload file, so the package has a data region to corrupt. An empty
    // package has nothing but its index, and breaking the index is a different
    // failure from breaking content.
    let payload = [Payload::named("app.exe", b"app payload", Some("core"))];
    let plan = plan(app, frontend, &payload, &[], scratch.path());
    let mut package = BundleWriter::encode(&plan, &[]).expect("the package encodes");
    let metadata_len =
        u64::from_le_bytes(package[20..28].try_into().expect("a length header")) as usize;
    let inside = 60 + metadata_len + 10;
    assert!(
        inside < package.len(),
        "the package is long enough to corrupt"
    );
    package[inside] ^= 0x40;
    let package_path = scratch.path().join("corrupt.zupbundle");
    std::fs::write(&package_path, package).expect("the package is written");

    let path = scratch.path().join("Setup.exe");
    let template = template(frontend);
    zup_windows::embed_bundle_file(&template, &path, &package_path, None)
        .expect("the package embeds");
    Setup { scratch, path }
}

/// The plan for one application, with its payload written beside it.
fn plan(
    app: &AppSpec,
    frontend: Frontend,
    payload: &[Payload],
    plugins: &[PluginSpec],
    scratch: &Path,
) -> TargetBuildPlan {
    let files = payload
        .iter()
        .map(|entry| {
            let source = scratch.join(&entry.name);
            std::fs::write(&source, &entry.bytes).expect("a payload file");
            let (size, sha256) = hash_reader(entry.bytes.as_slice()).expect("a payload hashes");
            ResolvedFile {
                source,
                source_relative: RelativePath::new(&entry.name).expect("a relative path"),
                destination: Template::parse(&entry.destination()).expect("a destination"),
                size,
                sha256,
                component: component_of(entry),
                condition: None,
            }
        })
        .collect::<Vec<_>>();
    let total_size = files.iter().map(|file| file.size).sum();
    let resolved_plugins = plugins
        .iter()
        .map(|plugin| {
            let bytes = std::fs::read(&plugin.source).expect("the plugin source is readable");
            let (size, sha256) = hash_reader(bytes.as_slice()).expect("the plugin hashes");
            ResolvedPlugin {
                id: zup_core::PluginId::new(&plugin.id).expect("a plugin id"),
                source: plugin.source.clone(),
                source_relative: RelativePath::new(&plugin.source_relative)
                    .expect("a relative path"),
                size,
                sha256,
            }
        })
        .collect::<Vec<_>>();
    TargetBuildPlan {
        installer: installer_ir(app, frontend, payload, plugins),
        prerequisites: Vec::new(),
        plugins: resolved_plugins,
        total_size,
        prerequisite_size: 0,
        icons: zup_core::TargetIcons::default(),
        files,
        ui_assets: Vec::new(),
    }
}

/// The installer IR for one application.
fn installer_ir(
    app: &AppSpec,
    frontend: Frontend,
    payload: &[Payload],
    plugins: &[PluginSpec],
) -> zup_core::Installer {
    zup_core::Installer {
        preset: None,
        app: App {
            id: AppId::new(&app.id).expect("a valid app id"),
            name: NonEmptyString::new(app.name.clone()).expect("a name"),
            version: semver::Version::parse(&app.version).expect("a version"),
            publisher: None,
            main: None,
            description: None,
        },
        target: host_target(),
        frontend,
        updates: None,
        install: Install {
            scope: InstallScope::User,
            directory: InstallDirectory {
                user: Some(Template::parse(&app.install_directory).expect("a directory")),
                machine: None,
            },
            allow_directory_override: false,
        },
        prerequisites: Vec::new(),
        components: components(payload),
        component_groups: Vec::new(),
        plugins: plugins
            .iter()
            .map(|plugin| zup_core::PluginBinding {
                id: zup_core::PluginId::new(&plugin.id).expect("a plugin id"),
                component: None,
                when: None,
            })
            .collect(),
        // The IR's file mappings are the *declarations* a manifest carries, with
        // their patterns unexpanded. The materialized inventory is `plan.files`.
        // Putting the expanded files in both would describe every destination
        // twice, and Windows lowering reads that as two files colliding on one
        // path.
        files: Vec::new(),
        launchers: Vec::new(),
        path: Vec::new(),
        services: Vec::new(),
        protocols: Vec::new(),
        file_associations: Vec::new(),
    }
}

fn component_of(entry: &Payload) -> Option<ComponentId> {
    entry
        .component
        .map(|id| ComponentId::new(id).expect("a component id"))
}

/// The components the payload needs, `core` first.
///
/// `core` is what an application cannot run without, so it is the one component a
/// person is not offered a way to leave out.
fn components(payload: &[Payload]) -> Vec<Component> {
    ["core", "docs"]
        .into_iter()
        .filter(|id| payload.iter().any(|entry| entry.component == Some(id)))
        .map(|id| Component {
            id: ComponentId::new(id).expect("a component id"),
            name: NonEmptyString::new(if id == "core" {
                "Core"
            } else {
                "Documentation"
            })
            .expect("a name"),
            description: None,
            required: id == "core",
            default: true,
            requires: Vec::new(),
            group: None,
        })
        .collect()
}

/// The machine this test host runs, which is the machine the staged templates are
/// built for.
pub fn host_target() -> TargetTriple {
    TargetTriple::parse(HOST_TARGET).expect("the host target is a valid triple")
}

/// The runtime image for one frontend, from the staged toolchain.
pub fn template(frontend: Frontend) -> PathBuf {
    let profile = profile_directory();
    let name = zup_toolchain::file_name(
        &zup_toolchain::ToolchainComponent::Runtime {
            target: host_target(),
            frontend,
        },
        EXECUTABLE_SUFFIX,
    );
    for root in [
        profile.join("toolchain").join(ZUP_VERSION),
        profile.join("toolchain"),
    ] {
        let path = root.join(&name);
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "no `{name}` in the staged toolchain beside {}.\n\n  \
         The runtime tests need real templates. Build them once:\n    \
         cargo xtask toolchain build",
        profile.display()
    );
}

/// The `target/<profile>` directory the test binary was built into.
fn profile_directory() -> PathBuf {
    let executable = std::env::current_exe().expect("a test executable");
    let mut directory = executable.parent().unwrap_or(&executable).to_path_buf();
    if directory.ends_with("deps") {
        directory.pop();
    }
    directory
}

/// The peer preset the end-to-end tests install.
///
/// Staged by the same run as every other real binary, and read here rather than
/// built: a test that shelled out to cargo would be a second build inside a
/// build, and the two tests that launch a peer would race for the same output.
pub fn test_preset() -> PathBuf {
    let profile = profile_directory();
    let name = zup_toolchain::test_preset_file_name(EXECUTABLE_SUFFIX);
    for root in [
        profile.join("toolchain").join(ZUP_VERSION),
        profile.join("toolchain"),
    ] {
        let path = root.join(&name);
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "no `{name}` in the staged toolchain beside {}.\n\n  \
         The end-to-end tests need a real peer preset. Build it once:\n    \
         cargo xtask toolchain build",
        profile.display()
    );
}

/// The zup version the staged components were built for. A component stamped with
/// any other version is refused by its descriptor, which is the contract rather
/// than something to work around.
const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The suffix this machine writes executables with.
const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

/// The canonical target triple of the build host.
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;
