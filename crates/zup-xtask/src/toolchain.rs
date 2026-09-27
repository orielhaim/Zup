//! Staging the local toolchain a contributor's builds compose from.
//!
//! `zup build` finds the runtime templates and dispatchers it needs by asking a
//! toolchain resolver, which looks in a fixed set of places and checks a
//! compatibility descriptor beside whatever it finds. None of that requires a
//! network, and none of it requires a contributor to know it exists.
//!
//! This is the step that *produces* a toolchain. It builds the three runtime
//! templates and the two dispatchers, names each one for the machine and
//! frontend it is for, writes its descriptor, and copies the whole set beside
//! `zup` so the resolver's staged-directory search finds it.
//!
//! Repository development and product use are different jobs with different
//! ergonomics, and both are meant to be good. A person who installed `zup` gets
//! a toolchain that shipped with it. A person working inside this repository runs
//! one command and gets a toolchain built from the source in front of them.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::dispatcher;

/// The package that produces the runtime templates.
pub const INSTALLER_PACKAGE: &str = "zup-installer";

/// The package that produces the dispatchers.
pub const DISPATCHER_PACKAGE: &str = "zup-dispatch";

/// The three runtime templates, as `(feature, frontend)`.
pub const FRONTENDS: &[(&str, &str)] = &[
    ("gui", "zup-setup-gui"),
    ("console", "zup-setup-console"),
    ("headless", "zup-setup-headless"),
];

/// The four dispatcher images, as `(feature, binary, subsystem)`.
///
/// The `online` flavour is the same source built with a feature, not a separate
/// binary, and it is named separately in the staging directory because the two
/// differ by megabytes and a build that measured the wrong one would report a
/// number nobody could reproduce.
pub const DISPATCHERS: &[(&str, &str, &str)] = &[
    ("", "zup-dispatch", "gui"),
    ("online", "zup-dispatch", "gui"),
    ("", "zup-dispatch-console", "console"),
    ("online", "zup-dispatch-console", "console"),
];

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
pub fn build(root: &Path, profile: &str) -> Result<Vec<PathBuf>, String> {
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
    Ok(written)
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
/// tree instead — "use release if `target/release` exists" — would stage a
/// toolchain one of the two could not see, which is the kind of failure that
/// disappears the next time anybody runs a release build.
pub fn staging_directory(root: &Path, profile: &str, version: &str) -> PathBuf {
    target_directory(root, profile)
        .join("toolchain")
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
fn machine_suffix() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
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
    let file_name = zup_toolchain::file_name(component, EXECUTABLE_SUFFIX);
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

/// The name a staged component was written under, for a report line.
fn component_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
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

    #[test]
    fn every_runtime_template_is_named_for_its_own_frontend() {
        let names: Vec<&str> = FRONTENDS.iter().map(|(_, binary)| *binary).collect();
        assert_eq!(
            names,
            vec!["zup-setup-gui", "zup-setup-console", "zup-setup-headless"]
        );
        for (feature, binary) in FRONTENDS {
            assert!(
                binary.ends_with(feature),
                "{binary} is not the {feature} template"
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
    fn a_staged_runtime_is_named_for_its_machine_and_its_frontend() {
        let component = runtime_component("gui");
        let name = zup_toolchain::file_name(&component, EXECUTABLE_SUFFIX);
        assert!(
            name.contains("gui") && name.contains(machine_suffix()),
            "{name}"
        );
        for (feature, binary) in FRONTENDS {
            assert!(
                binary.ends_with(feature),
                "{binary} is not the {feature} template"
            );
        }
    }

    #[test]
    fn the_two_launcher_flavours_get_separate_staged_names() {
        // The same binary built with and without the online path differs by
        // megabytes, and a staged tree that could not tell them apart would
        // compose a thin installer that cannot install itself.
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
}
