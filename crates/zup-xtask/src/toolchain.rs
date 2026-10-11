use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use crate::dispatcher;

pub const INSTALLER_PACKAGE: &str = "zup-installer";

pub const DISPATCHER_PACKAGE: &str = "zup-dispatch";

pub const CLI_PACKAGE: &str = "zup";

pub const PRESET_PACKAGE: &str = "zup-preset-default";

pub const PRESET_DIRECTORY: &str = "crates/zup-preset-default";

pub const TEST_PRESET_PACKAGE: &str = "zup-preset-test";

#[cfg(windows)]
pub const FRONTENDS: &[(&str, &str)] = &[
    ("gui", "zup-setup-gui"),
    ("console", "zup-setup-console"),
    ("headless", "zup-setup-headless"),
];

#[cfg(not(windows))]
pub const FRONTENDS: &[(&str, &str)] = &[
    ("console", "zup-setup-console"),
    ("headless", "zup-setup-headless"),
];

#[cfg(windows)]
pub const DISPATCHERS: &[(&str, &str, &str)] = &[
    ("", "zup-dispatch", "gui"),
    ("online", "zup-dispatch", "gui"),
    ("", "zup-dispatch-console", "console"),
    ("online", "zup-dispatch-console", "console"),
];

pub const STAGED_DIRECTORY: &str = "toolchain";

const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

pub fn version() -> Result<String, String> {
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
        let built = target_directory(root, dispatcher::TARGET)
            .join(output_directory(profile))
            .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        cargo_build(root, &args, LAUNCHER_PROFILE)?;
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

const SUPPORTED_CROSS_TARGET: &str = "x86_64-unknown-linux-gnu";

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

#[cfg(windows)]
fn build_and_stage_preset(
    root: &Path,
    staged: &Path,
    version: &str,
    profile: &str,
) -> Result<PathBuf, String> {
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

#[cfg(windows)]
const LAUNCHER_PROFILE: &[(&str, &str)] = &[
    ("CARGO_PROFILE_RELEASE_LTO", "fat"),
    ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "z"),
    ("CARGO_PROFILE_RELEASE_PANIC", "abort"),
    ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1"),
];

pub fn staging_directory(root: &Path, profile: &str, version: &str) -> PathBuf {
    target_directory(root, profile)
        .join(STAGED_DIRECTORY)
        .join(version)
}

fn target_directory(root: &Path, profile: &str) -> PathBuf {
    root.join("target").join(output_directory(profile))
}

fn output_directory(profile: &str) -> &str {
    match profile {
        "dev" => "debug",
        other => other,
    }
}

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

fn stage_component(
    built: &Path,
    staged: &Path,
    component: &zup_toolchain::ToolchainComponent,
    version: &str,
) -> Result<PathBuf, String> {
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

fn component_suffix(component: &zup_toolchain::ToolchainComponent) -> &'static str {
    match component {
        zup_toolchain::ToolchainComponent::Runtime { target, .. } => target.executable_suffix(),
        zup_toolchain::ToolchainComponent::Dispatcher { .. } => ".exe",
        zup_toolchain::ToolchainComponent::Preset => "",
    }
}

fn component_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub use zup_toolchain::RELEASE_INDEX_NAME;

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

    let mut components = Vec::new();
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
