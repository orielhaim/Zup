//! Staging the local toolchain a contributor's builds compose from.
//!
//! `zup build` finds the runtime templates and dispatchers it needs by asking a
//! toolchain resolver, which looks in a fixed set of places and checks a
//! compatibility descriptor beside whatever it finds. None of that requires a
//! network, and none of it requires a contributor to know it exists.
//!
//! This is the step that *produces* a toolchain. On Windows it builds the three
//! runtime templates, the dispatchers, and the preset package, names each one
//! for the machine and frontend it is for, writes its descriptor, and copies
//! the whole set beside `zup` so the resolver's staged-directory search finds
//! it. On Linux it builds the console and headless runtimes and stops there:
//! there is nothing to dispatch through and no preset host. With `--target
//! x86_64-unknown-linux-gnu` it cross-builds the Linux runtimes from another
//! host with `cargo zigbuild`, so a Windows machine can stage what a Linux
//! `zup build` composes.
//!
//! Repository development and product use are different jobs with different
//! ergonomics, and both are meant to be good. A person who installed `zup` gets
//! a toolchain that shipped with it. A person working inside this repository runs
//! one command and gets a toolchain built from the source in front of them.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use crate::dispatcher;

/// The package that produces the runtime templates.
pub const INSTALLER_PACKAGE: &str = "zup-installer";

/// The package that produces the dispatchers.
pub const DISPATCHER_PACKAGE: &str = "zup-dispatch";

/// The package that produces the developer CLI, and the only binary a person
/// installs.
pub const CLI_PACKAGE: &str = "zup";

/// The package that produces the default preset.
///
/// Published like any other preset: it is a separate process the installer
/// launches, so the toolchain stages a `.zupui` of it exactly as a build
/// consumes a `.zupui` a user chose.
pub const PRESET_PACKAGE: &str = "zup-preset-default";

/// Where that package's project lives, relative to the repository root.
pub const PRESET_DIRECTORY: &str = "crates/zup-preset-default";

/// The peer preset the runtime's end-to-end tests install.
pub const TEST_PRESET_PACKAGE: &str = "zup-preset-test";

/// The three runtime templates, as `(feature, frontend)`.
///
/// Windows stages all three; Linux stages console and headless only. A GUI
/// template on Linux would be a binary whose only behavior is refusing to
/// run, and staging it would let a build resolve a "Linux GUI runtime" that
/// cannot install anything.
#[cfg(windows)]
pub const FRONTENDS: &[(&str, &str)] = &[
    ("gui", "zup-setup-gui"),
    ("console", "zup-setup-console"),
    ("headless", "zup-setup-headless"),
];

/// The runtime templates a Linux host stages: console and headless.
///
/// No GUI, no dispatchers, no presets: the Linux Phase 2 installer is a
/// self-contained binary run directly, not a dispatcher/launcher/preset host.
#[cfg(not(windows))]
pub const FRONTENDS: &[(&str, &str)] = &[
    ("console", "zup-setup-console"),
    ("headless", "zup-setup-headless"),
];

/// The four dispatcher images, as `(feature, binary, subsystem)`.
///
/// Windows-only: dispatchers are launcher images, and a Linux toolchain has
/// nothing to dispatch through.
#[cfg(windows)]
pub const DISPATCHERS: &[(&str, &str, &str)] = &[
    ("", "zup-dispatch", "gui"),
    ("online", "zup-dispatch", "gui"),
    ("", "zup-dispatch-console", "console"),
    ("online", "zup-dispatch-console", "console"),
];

/// The directory a staged toolchain lives in, under a profile directory.
///
/// One constant because the developer-side resolver, the staging step, and the
/// test harnesses all have to agree on it, and three independent spellings of the
/// same path is how every composition test ends up reporting a missing template
/// that is not actually missing.
pub const STAGED_DIRECTORY: &str = "toolchain";

/// The suffix the machine staging a toolchain writes executables with, which is
/// part of the name a component is stored under.
const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

/// The zup version a staged toolchain belongs to.
/// A staged toolchain is only usable by the zup release that staged it, and the
/// resolver enforces that through each component's descriptor. Naming the
/// directory after the version means two releases can be staged side by side on
/// one machine without either of them finding the other's bytes.
pub fn version() -> Result<String, String> {
    // Read rather than compile in: the xtask binary is built once and the version
    // it reports has to be the version of the workspace it is running in.
    let root = repository_root();
    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).map_err(|error| {
        format!(
            "`{}`: {error}; run this from the zup repository",
            manifest.display()
        )
    })?;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("version") else {
            continue;
        };
        let value = rest.trim_start().trim_start_matches('=').trim();
        if !value.is_empty() {
            return Ok(value.trim_matches('"').to_owned());
        }
    }
    Err(format!(
        "`{}` declares no workspace version",
        manifest.display()
    ))
}

/// Build the toolchain and stage it beside `zup`.
///
/// `target` names a cross-build target, and `None` means this host. A cross
/// target is how a Windows host stages the Linux runtime templates a Linux
/// `zup build` composes: the templates are built with `cargo zigbuild`, which
/// owns the cross linker, and staged under the same target-qualified names a
/// native build stages. Only Linux cross targets are supported: dispatchers
/// and presets are Windows launcher machinery with no cross equivalent in this
/// phase.
pub fn build(root: &Path, profile: &str, target: Option<&str>) -> Result<Vec<PathBuf>, String> {
    if let Some(target) = target {
        return build_cross(root, profile, target);
    }
    let version = version()?;
    let staged = staging_directory(root, profile, &version);
    std::fs::create_dir_all(&staged).map_err(|error| format!("{}: {error}", staged.display()))?;
    let mut written = Vec::new();

    for (feature, binary) in FRONTENDS {
        cargo(
            root,
            &[
                "build",
                "-p",
                INSTALLER_PACKAGE,
                "--no-default-features",
                "--features",
                feature,
                "--bin",
                binary,
                "--profile",
                profile,
            ],
        )?;
        let built = target_directory(root, profile)
            .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        let component = runtime_component(feature);
        let staged_file = stage_component(&built, &staged, &component, &version)?;
        println!("  runtime  {feature:<8} {}", component_name(&staged_file));
        written.push(staged_file);
    }

    // Dispatchers are Windows launcher images. A Linux toolchain has nothing
    // to dispatch through: the installer binary runs directly.
    #[cfg(windows)]
    for (feature, binary, subsystem) in DISPATCHERS {
        let online = !feature.is_empty();
        let mut args = vec![
            "build".to_owned(),
            "-p".to_owned(),
            DISPATCHER_PACKAGE.to_owned(),
            "--bin".to_owned(),
            (*binary).to_owned(),
            "--target".to_owned(),
            dispatcher::TARGET.to_owned(),
            "--profile".to_owned(),
            profile.to_owned(),
        ];
        if online {
            args.push("--features".to_owned());
            args.push((*feature).to_owned());
        }
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        // A cross-compiled build writes to `target/<triple>/<profile>`, not to the
        // host profile's directory with a subdirectory in it.
        let built = target_directory(root, dispatcher::TARGET)
            .join(output_directory(profile))
            .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        cargo_build(root, &args, LAUNCHER_PROFILE)?;
        // The two flavours are the same binary name in the same output directory,
        // so each is staged before the next is built. The staged name is where
        // the `online` distinction lives, because a build that overwrote one with
        // the other would produce a thin installer that cannot install itself.
        let component = zup_toolchain::ToolchainComponent::Dispatcher {
            subsystem: match *subsystem {
                "gui" => zup_toolchain::Subsystem::Gui,
                _ => zup_toolchain::Subsystem::Console,
            },
            online,
        };
        let staged_file = stage_component(&built, &staged, &component, &version)?;
        println!("  dispatch {subsystem:<8} {}", component_name(&staged_file));
        written.push(staged_file);
    }

    // Presets, the test peer, and the example plugin are Windows-presentation
    // machinery: a preset host, a window peer, and a demo plugin. None of them
    // is part of a Linux console/headless installer, so a Linux toolchain
    // stages runtimes and stops.
    #[cfg(windows)]
    {
        let preset = build_and_stage_preset(root, &staged, &version, profile)?;
        println!("  preset             {}", component_name(&preset));
        written.push(preset);

        let peer = build_and_stage_test_preset(root, &staged, profile)?;
        println!("  preset-peer        {}", component_name(&peer));
        written.push(peer);

        build_example_plugin(root)?;
    }
    Ok(written)
}

/// Stage runtime templates for a Linux target from another host.
///
/// The application developer never runs this: `zup build --target
/// x86_64-unknown-linux-gnu` resolves its templates through the toolchain
/// model, and this is the contributor-side producer that puts them where the
/// resolver looks. `cargo zigbuild` owns the cross linker; this owns which
/// binaries are built, what they are staged as, and the descriptors that make
/// them usable.
fn build_cross(root: &Path, profile: &str, target: &str) -> Result<Vec<PathBuf>, String> {
    let component_target =
        zup_core::TargetTriple::parse(target).map_err(|error| format!("`{target}`: {error}"))?;
    if component_target.operating_system() != zup_core::TargetOperatingSystem::Linux {
        return Err(format!(
            "cross toolchain builds support Linux targets; `{target}` is not one"
        ));
    }
    if component_target.architecture() != zup_core::TargetArchitecture::X86_64 {
        return Err(format!(
            "cross toolchain builds support `{SUPPORTED_CROSS_TARGET}`; `{target}` is not one"
        ));
    }
    let version = version()?;
    let staged = staging_directory(root, profile, &version);
    std::fs::create_dir_all(&staged).map_err(|error| format!("{}: {error}", staged.display()))?;
    let mut written = Vec::new();
    for frontend in ["console", "headless"] {
        let binary = format!("zup-setup-{frontend}");
        zigbuild(
            root,
            &[
                "--target",
                target,
                "-p",
                INSTALLER_PACKAGE,
                "--no-default-features",
                "--features",
                frontend,
                "--bin",
                &binary,
                "--profile",
                profile,
            ],
        )?;
        let built = target_directory(root, target)
            .join(output_directory(profile))
            .join(&binary);
        let component = zup_toolchain::ToolchainComponent::Runtime {
            target: component_target.clone(),
            frontend: match frontend {
                "console" => zup_core::Frontend::Console,
                _ => zup_core::Frontend::Headless,
            },
        };
        let staged_file = stage_component(&built, &staged, &component, &version)?;
        println!("  runtime  {frontend:<8} {}", component_name(&staged_file));
        written.push(staged_file);
    }
    Ok(written)
}

/// The one cross-build target a contributor can stage from another host.
const SUPPORTED_CROSS_TARGET: &str = "x86_64-unknown-linux-gnu";

/// Run one `cargo zigbuild` command.
///
/// `cargo zigbuild` is the cross linker this repository's Linux templates are
/// built with from other hosts. It needs `zig` beside it; when it is missing
/// the refusal names it rather than reporting a linker error.
fn zigbuild(root: &Path, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new(cargo_executable());
    command.current_dir(root).arg("zigbuild");
    for argument in args {
        command.arg(argument);
    }
    let status = command.status().map_err(|error| {
        format!(
            "run `cargo zigbuild {}`: {error}; `cargo zigbuild` needs `zig` on PATH",
            args.join(" ")
        )
    })?;
    if !status.success() {
        return Err(format!("`cargo zigbuild {}` failed", args.join(" ")));
    }
    Ok(())
}

#[cfg(windows)]
/// The example plugin, built the way an author builds one.
///
/// Not staged as a toolchain component, because it is not one: nothing an
/// installer ships reads it. It is built here because a test that loads it has
/// to be loading something current. Built through `zup plugin build` rather than
/// by invoking cargo and a componentiser directly, so the path those tests
/// depend on is the path an author produces.
///
/// Windows-only with the rest of the plugin demo machinery: no Linux
/// installer in this phase plans a plugin.
#[cfg(windows)]
fn build_example_plugin(root: &Path) -> Result<(), String> {
    let project = root.join("examples").join("plugins").join("configure");
    if !project.is_dir() {
        return Err(format!("no example plugin at {}", project.display()));
    }
    cargo(
        root,
        &[
            "run",
            "--quiet",
            "-p",
            "zup",
            "--",
            "plugin",
            "build",
            "--project",
            &project.to_string_lossy(),
        ],
    )
}

/// Build and stage the peer preset the runtime's end-to-end tests install.
///
/// A real preset, written against the public SDK, staged beside the components a
/// build host needs. It is staged rather than built by the test that uses it so
/// that the whole test run is one build: a test that shelled out to cargo would
/// be a second build inside a build, and two tests doing it at once would race
/// for the same output.
///
/// Windows-only: the test peer is a window the runtime's end-to-end tests
/// launch, and Linux console/headless installers present no window.
#[cfg(windows)]
fn build_and_stage_test_preset(
    root: &Path,
    staged: &Path,
    profile: &str,
) -> Result<PathBuf, String> {
    let binary = zup_toolchain::test_preset_file_name("");
    cargo(
        root,
        &[
            "build",
            "-p",
            TEST_PRESET_PACKAGE,
            "--bin",
            &binary,
            "--profile",
            profile,
        ],
    )?;
    let built = target_directory(root, profile).join(format!("{binary}{EXECUTABLE_SUFFIX}"));
    let name = zup_toolchain::test_preset_file_name(EXECUTABLE_SUFFIX);
    let destination = staged.join(&name);
    std::fs::copy(&built, &destination)
        .map_err(|error| format!("{}: {error}", destination.display()))?;
    Ok(destination)
}

/// Published through `zup preset pack` rather than by writing a package here, so the
/// preset Zup ships is produced by the same publisher a third-party author uses
/// and a build that consumes it is consuming something a user could have
/// downloaded. The package carries every target it was built for, so the staged
/// component is one file rather than one per target.
///
/// Windows-only: presets are presented windows, which Linux Phase 2 does not have.
#[cfg(windows)]
fn build_and_stage_preset(
    root: &Path,
    staged: &Path,
    version: &str,
    profile: &str,
) -> Result<PathBuf, String> {
    // Somewhere else, because staging replaces the file it is given: packing
    // straight into the staging directory would have the stager delete the
    // package and then look for it.
    let scratch = tempfile::tempdir_in(staged)
        .map_err(|error| format!("a scratch directory in {}: {error}", staged.display()))?;
    let package = scratch.path().join(zup_toolchain::PRESET_PACKAGE_NAME);
    cargo(
        root,
        &[
            "run",
            "-p",
            CLI_PACKAGE,
            "--profile",
            profile,
            "--",
            "preset",
            "pack",
            "--manifest",
            &root.join(PRESET_DIRECTORY).to_string_lossy(),
            "--build",
            machine_suffix(),
            "--profile",
            profile,
            "--output",
            &package.to_string_lossy(),
            "--force",
        ],
    )?;
    stage_component(
        &package,
        staged,
        &zup_toolchain::ToolchainComponent::Preset,
        version,
    )
}

/// The release-profile settings a launcher is built with.
///
/// A launcher's size is a design constraint, and it is the one component a client
/// downloads before it has needed any of it. They are passed to the one cargo
/// invocation that needs them rather than set in this process's environment,
/// because Cargo has no per-package profile and because mutating the environment
/// of a process that may be reading it on another thread is not a thing to do for
/// a build setting.
///
/// `-C lto` must not reach RUSTFLAGS: it conflicts with the bitcode settings LTO
/// needs.
#[cfg(windows)]
const LAUNCHER_PROFILE: &[(&str, &str)] = &[
    ("CARGO_PROFILE_RELEASE_LTO", "fat"),
    ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "z"),
    ("CARGO_PROFILE_RELEASE_PANIC", "abort"),
    ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1"),
];

/// Where a staged toolchain for one profile and one version lives beside `zup`.
///
/// The profile is part of the path because the resolver's staged-directory search
/// starts beside the executable it is running as, and a release `zup` and a debug
/// `zup` do not sit in the same directory. Reading the profile out of the target
/// tree instead - "use release if `target/release` exists" - would stage a
/// toolchain one of the two could not see, which is the kind of failure that
/// disappears the next time anybody runs a release build.
pub fn staging_directory(root: &Path, profile: &str, version: &str) -> PathBuf {
    target_directory(root, profile)
        .join(STAGED_DIRECTORY)
        .join(version)
}

/// The directory Cargo writes one profile's build output into.
fn target_directory(root: &Path, profile: &str) -> PathBuf {
    root.join("target").join(output_directory(profile))
}

/// The `target/<name>` directory a Cargo profile writes into.
///
/// Cargo names the development profile `dev` and the directory `debug`, and the
/// resolver searches the directory. The mapping lives here so the two spellings
/// cannot drift, which would stage a toolchain under a name nothing looks for.
fn output_directory(profile: &str) -> &str {
    match profile {
        "dev" => "debug",
        other => other,
    }
}

/// The machine this host is, in the spelling a component file name uses.
///
/// The host's own triple, not a guess: a toolchain stages runtimes for the
/// machine that built them, and a Linux host stages Linux runtimes the same
/// way a Windows host stages Windows ones.
fn machine_suffix() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-pc-windows-msvc"
        } else {
            "x86_64-pc-windows-msvc"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

/// The runtime component for one frontend, built for this host.
fn runtime_component(frontend: &str) -> zup_toolchain::ToolchainComponent {
    zup_toolchain::ToolchainComponent::Runtime {
        target: zup_core::TargetTriple::parse(machine_suffix())
            .expect("the host's own target triple is valid"),
        frontend: match frontend {
            "gui" => zup_core::Frontend::Gui,
            "console" => zup_core::Frontend::Console,
            _ => zup_core::Frontend::Headless,
        },
    }
}

/// Copy one built component into the staging directory under its real name, and
/// write the descriptor that makes it usable.
///
/// The staged name carries the component's own target suffix, not the staging
/// host's: a Linux template staged from Windows is extensionless, exactly as a
/// native Linux build stages it, because the resolver searches for that name.
fn stage_component(
    built: &Path,
    staged: &Path,
    component: &zup_toolchain::ToolchainComponent,
    version: &str,
) -> Result<PathBuf, String> {
    // The name comes from the contract, not from the binary cargo happened to
    // write. A stager that composed its own name from the binary's would produce
    // a toolchain the resolver cannot find, and the failure would be a missing
    // file rather than a wrong one.
    let file_name = zup_toolchain::file_name(component, component_suffix(component));
    let destination = staged.join(&file_name);
    let _ = std::fs::remove_file(&destination);
    std::fs::copy(built, &destination).map_err(|error| {
        format!(
            "stage `{}`: {error}; the build should have written it",
            built.display()
        )
    })?;
    let descriptor = zup_toolchain::ComponentDescriptor::of(component, version, &destination)
        .map_err(|error| format!("describe `{file_name}`: {error}"))?;
    let descriptor_path = staged.join(format!("{file_name}{}", zup_toolchain::DESCRIPTOR_SUFFIX));
    std::fs::write(&descriptor_path, descriptor.encode())
        .map_err(|error| format!("{}: {error}", descriptor_path.display()))?;
    Ok(destination)
}

/// The suffix a staged component is stored under: its own target's, never the
/// staging host's.
///
/// A Linux template staged from Windows is extensionless, exactly as a native
/// Linux build stages it. Staging it with the host's `.exe` would produce a
/// toolchain the resolver - which searches for the component's own name - can
/// never find.
fn component_suffix(component: &zup_toolchain::ToolchainComponent) -> &'static str {
    match component {
        zup_toolchain::ToolchainComponent::Runtime { target, .. } => target.executable_suffix(),
        // Dispatchers are Windows launcher images, built on Windows for Windows.
        zup_toolchain::ToolchainComponent::Dispatcher { .. } => ".exe",
        // A package holds every target, so no executable suffix.
        zup_toolchain::ToolchainComponent::Preset => "",
    }
}

/// The name a staged component was written under, for a report line.
fn component_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The index file name a release carries.
///
/// Re-exported from the contract crate rather than spelled here: the packaging
/// step writes it, the clean room reads it, and `zup toolchain install` reads it
/// to populate a cache. Three spellings of one file name is a release that
/// verifies against nothing.
pub use zup_toolchain::RELEASE_INDEX_NAME;

/// Assemble one directory that is a complete, self-describing zup release.
///
/// The shape is the whole design:
///
/// ```text
/// <out>/
///   zup.exe                       the developer CLI
///   toolchain/<version>/…         the three runtime templates and four launchers,
///                                 each with the descriptor beside it
///   zup-toolchain.json            the index: every file, with its digest
/// ```
///
/// A developer unzips this and runs `zup build`. There is no second download, no
/// six manual steps, and nothing to install: the resolver already searches
/// `<exe_dir>/toolchain/<version>`, which is exactly where this puts the
/// components, so a release *is* the thing a build consumes.
///
/// Everything is copied out of the profile's staging directory rather than built
/// again, so packaging is a file operation and cannot disagree with a build that
/// already happened.
pub fn package(root: &Path, profile: &str, out: &Path) -> Result<PathBuf, String> {
    let version = version()?;
    let staged = staging_directory(root, profile, &version);
    if !staged.is_dir() {
        return Err(format!(
            "there is no {} toolchain in {}; run `cargo xtask toolchain build --profile {profile}` first",
            profile,
            staged.display()
        ));
    }
    std::fs::create_dir_all(out).map_err(|error| format!("{}: {error}", out.display()))?;

    // The CLI, built for the same profile as the components it composes with.
    cargo(
        root,
        &[
            "build",
            "-p",
            CLI_PACKAGE,
            "--bin",
            CLI_PACKAGE,
            "--profile",
            profile,
        ],
    )?;
    let built = target_directory(root, profile).join(format!("{CLI_PACKAGE}{EXECUTABLE_SUFFIX}"));
    copy(
        &built,
        &out.join(format!("{CLI_PACKAGE}{EXECUTABLE_SUFFIX}")),
    )?;

    // Every staged file, under its own name, in the versioned directory the
    // resolver looks in. Descriptors are indexed too: a descriptor is a
    // component's claim about itself, and an index that proved the executable
    // without proving the claim would be asserting half of what the release
    // ships.
    let mut components = Vec::new();
    // The peer preset the runtime's end-to-end tests launch is staged beside the
    // components so that a test finds it where every other real binary is, and is
    // deliberately not one of them: `zup_toolchain::host_components` does not
    // list it, and it carries no descriptor because it is not a component.
    // Indexing it would put a test binary in every release and assert a claim the
    // release does not make, which the self-consistency check below is right to
    // refuse.
    let test_peer = zup_toolchain::test_preset_file_name(EXECUTABLE_SUFFIX);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&staged)
        .map_err(|error| format!("{}: {error}", staged.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| component_name(path) != test_peer)
        .collect();
    entries.sort();
    for entry in entries {
        let name = component_name(&entry);
        let relative = format!("{STAGED_DIRECTORY}/{version}/{name}");
        let destination = out.join(&relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        copy(&entry, &destination)?;
        components.push(
            zup_toolchain::ReleaseFile::of(&relative, &destination)
                .map_err(|error| format!("describe `{relative}`: {error}"))?,
        );
    }
    components.sort_by(|left, right| left.path.cmp(&right.path));

    let mut index = zup_toolchain::ToolchainRelease::new(version.clone(), machine_target());
    index.cli = zup_toolchain::ReleaseFile::of(
        &format!("{CLI_PACKAGE}{EXECUTABLE_SUFFIX}"),
        &out.join(format!("{CLI_PACKAGE}{EXECUTABLE_SUFFIX}")),
    )
    .map_err(|error| format!("describe the CLI: {error}"))?;
    index.components = components;

    // The index is written last and then read back, so what this reports is what
    // the directory actually holds rather than what the copies were supposed to.
    let index_path = out.join(RELEASE_INDEX_NAME);
    std::fs::write(&index_path, index.encode())
        .map_err(|error| format!("{}: {error}", index_path.display()))?;
    index.verify(out).map_err(|error| {
        format!(
            "the release at {} is not self-consistent: {error}",
            out.display()
        )
    })?;
    Ok(out.to_path_buf())
}

/// The target a release's components are built for.
///
/// A release is one build on one machine, so this is the machine that built it
/// rather than the machine that happens to be unpacking it. A developer on
/// another machine downloads the other release.
///
/// The machine is the one this process runs on, read through `zup-binary` rather
/// than from `cfg!(target_arch)`: a release has to name a machine its components
/// can actually run on, and that is the same question `compose_universal_executable`
/// asks when it compares a dispatcher's width against a variant's.
pub fn machine_target() -> String {
    let machine = zup_binary::BinaryArchitecture::host().unwrap_or_else(|| {
        panic!(
            "this host runs on a machine zup has no Windows target triple for. A release has \
             to name a machine its components can run on, and there is no honest name for \
             one that has none."
        )
    });
    match machine {
        zup_binary::BinaryArchitecture::X86_64 => "x86_64-pc-windows-msvc".to_owned(),
        zup_binary::BinaryArchitecture::Arm64 => "aarch64-pc-windows-msvc".to_owned(),
        other => panic!(
            "{} is not a machine a Windows toolchain release targets",
            other
        ),
    }
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(to);
    std::fs::copy(from, to)
        .map_err(|error| format!("`{}` -> `{}`: {error}", from.display(), to.display()))?;
    Ok(())
}

fn cargo(root: &Path, args: &[&str]) -> Result<(), String> {
    cargo_build(root, args, &[])
}

/// Run one cargo command, with extra environment for that invocation only.
fn cargo_build(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
    let mut command = Command::new(cargo_executable());
    command.current_dir(root);
    for (name, value) in env {
        command.env(name, value);
    }
    for argument in args {
        command.arg(argument);
    }
    let status = command
        .status()
        .map_err(|error| format!("run cargo {}: {error}", args.join(" ")))?;
    if !status.success() {
        return Err(format!("`cargo {}` failed", args.join(" ")));
    }
    Ok(())
}

fn cargo_executable() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

/// The workspace containing this xtask, found by walking up to the manifest that
/// declares `[workspace]`.
pub fn repository_root() -> PathBuf {
    let mut directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if let Ok(manifest) = crate::workspace::read_manifest(&directory.join("Cargo.toml"))
            && manifest.get("workspace").is_some()
        {
            return directory;
        }
        let Some(parent) = directory.parent() else {
            panic!(
                "no [workspace] manifest above {}",
                env!("CARGO_MANIFEST_DIR")
            );
        };
        directory = parent.to_path_buf();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release ships the components a build composes into an installer, and
    /// nothing else.
    ///
    /// The peer preset the runtime's end-to-end tests launch is staged in the same
    /// directory so a test finds it there, and is deliberately not a component:
    /// `host_components` does not list it and it carries no descriptor. Indexing
    /// it put a test binary in every release and made the release fail its own
    /// self-consistency check, which is how this was found.
    #[test]
    fn a_release_does_not_ship_the_test_peer() {
        let peer = zup_toolchain::test_preset_file_name(EXECUTABLE_SUFFIX);
        assert_eq!(
            peer,
            format!("zup-preset-test{EXECUTABLE_SUFFIX}"),
            "the peer is named by the contract crate, so this test would notice it moving"
        );
        let listed = zup_toolchain::host_components(
            &zup_core::TargetTriple::parse(machine_suffix()).expect("a valid triple"),
        );
        for component in &listed {
            assert_ne!(
                zup_toolchain::file_name(component, EXECUTABLE_SUFFIX),
                peer,
                "a component list that included the peer would put it in a release"
            );
        }
    }

    /// A staged toolchain is a directory a build writes and a later composition
    /// reads. Keying it by profile and version is what lets a debug and a release
    /// build coexist, and what stops two releases from finding each other's bytes.
    #[test]
    fn a_staged_toolchain_is_keyed_by_the_profile_and_the_version_that_produced_it() {
        let root = Path::new("repo");
        let directory = staging_directory(root, "dev", "0.1.0");
        assert!(
            directory.ends_with("toolchain/0.1.0"),
            "{}",
            directory.display()
        );
        assert!(
            directory.starts_with("repo/target/debug"),
            "the development profile's directory is `debug`: {}",
            directory.display()
        );
        assert!(
            staging_directory(root, "release", "0.1.0").starts_with("repo/target/release"),
            "a release zup does not look beside a debug one"
        );
        assert!(
            !staging_directory(root, "dev", "0.2.0").ends_with("toolchain/0.1.0"),
            "two releases must be able to coexist without finding each other's bytes"
        );
    }

    /// A staged runtime is addressed by name, and the name is what a host later
    /// resolves. Two runtimes sharing one name is a tree that cannot tell them
    /// apart - for the offline and online launchers that means a thin installer
    /// which composes itself out of the wrong binary.
    #[test]
    fn the_two_launcher_flavours_get_separate_staged_names() {
        let offline = zup_toolchain::ToolchainComponent::Dispatcher {
            subsystem: zup_toolchain::Subsystem::Gui,
            online: false,
        };
        let online = zup_toolchain::ToolchainComponent::Dispatcher {
            subsystem: zup_toolchain::Subsystem::Gui,
            online: true,
        };
        assert_ne!(
            zup_toolchain::file_name(&offline, EXECUTABLE_SUFFIX),
            zup_toolchain::file_name(&online, EXECUTABLE_SUFFIX)
        );
    }

    /// A cross-staged Linux toolchain is extensionless and holds exactly the
    /// components the contract names for that target - no GUI runtime, no
    /// dispatchers, no preset - whatever host staged it.
    #[test]
    fn a_cross_staged_linux_toolchain_is_named_by_its_target() {
        let target = zup_core::TargetTriple::parse(SUPPORTED_CROSS_TARGET).expect("a Linux target");
        let wanted: Vec<String> = zup_toolchain::supported_components(&target)
            .iter()
            .map(|component| zup_toolchain::file_name(component, component_suffix(component)))
            .collect();
        assert_eq!(
            wanted,
            vec![
                format!("zup-setup-console-{SUPPORTED_CROSS_TARGET}"),
                format!("zup-setup-headless-{SUPPORTED_CROSS_TARGET}"),
            ],
            "console and headless, extensionless, and nothing else"
        );
        for name in &wanted {
            assert!(
                !name.ends_with(".exe"),
                "a Linux component carries no executable suffix: {name}"
            );
        }
    }

    /// The stager and the contract must agree on what a host needs.
    ///
    /// `FRONTENDS`, `DISPATCHERS` and `PRESET_PACKAGE` say which cargo
    /// invocation produces each component; `supported_components` says which
    /// components that is. They are two lists because one is a build plan
    /// and the other is a contract, and they are checked against each other here
    /// because a build that stages seven of the eight components succeeds at
    /// staging and fails at the first composition that needs the eighth.
    #[test]
    fn the_stager_produces_exactly_the_components_the_contract_names() {
        let target = zup_core::TargetTriple::parse(machine_suffix())
            .expect("the host's own target triple is valid");
        let wanted: Vec<String> = zup_toolchain::supported_components(&target)
            .iter()
            .map(|component| zup_toolchain::file_name(component, EXECUTABLE_SUFFIX))
            .collect();

        let mut staged: Vec<String> = FRONTENDS
            .iter()
            .map(|(feature, _)| {
                zup_toolchain::file_name(&runtime_component(feature), EXECUTABLE_SUFFIX)
            })
            .collect();
        // Dispatchers and the preset host are Windows launcher machinery: a
        // Linux toolchain stages runtimes and nothing else, and the contract
        // above agrees.
        #[cfg(windows)]
        {
            staged.extend(DISPATCHERS.iter().map(|(feature, _, subsystem)| {
                zup_toolchain::file_name(
                    &zup_toolchain::ToolchainComponent::Dispatcher {
                        subsystem: if *subsystem == "gui" {
                            zup_toolchain::Subsystem::Gui
                        } else {
                            zup_toolchain::Subsystem::Console
                        },
                        online: !feature.is_empty(),
                    },
                    EXECUTABLE_SUFFIX,
                )
            }));
            staged.push(zup_toolchain::file_name(
                &zup_toolchain::ToolchainComponent::Preset,
                EXECUTABLE_SUFFIX,
            ));
        }
        staged.sort();
        let mut wanted = wanted;
        wanted.sort();
        assert_eq!(staged, wanted);
    }
}
