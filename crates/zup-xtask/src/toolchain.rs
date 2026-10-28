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

pub fn frontends_for(target: &zup_core::Target) -> Vec<(&'static str, &'static str)> {
    zup_platform::capabilities(target)
        .map(|caps| caps.frontends)
        .unwrap_or_default()
        .iter()
        .map(|frontend| match frontend {
            zup_core::Frontend::Gui => ("gui", "zup-setup-gui"),
            zup_core::Frontend::Console => ("console", "zup-setup-console"),
            zup_core::Frontend::Headless => ("headless", "zup-setup-headless"),
        })
        .collect()
}

pub const DISPATCHERS: &[(&str, &str, &str)] = &[
    ("", "zup-dispatch", "gui"),
    ("", "zup-dispatch-console", "console"),
    ("online", "zup-dispatch", "gui"),
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
    let host = zup_core::Target::host().map_err(|error| error.to_string())?;
    if zup_platform::capabilities(&host).is_none() {
        return Err(format!("unsupported toolchain host `{host}`"));
    }

    for (feature, binary) in frontends_for(&host) {
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
        let component = runtime_component(host, feature);
        let staged_file = stage_component(&built, &staged, &component, &version)?;
        println!("  runtime  {feature:<8} {}", component_name(&staged_file));
        written.push(staged_file);
    }

    #[cfg(windows)]
    for (feature, binary, subsystem) in DISPATCHERS {
        written.push(stage_dispatcher(
            root, profile, &staged, &version, feature, binary, subsystem,
        )?);
    }

    {
        let preset = build_and_stage_preset(root, &staged, &version, profile)?;
        println!("  preset             {}", component_name(&preset));
        written.push(preset);

        let peer = build_and_stage_test_preset(root, &staged, profile)?;
        println!("  preset-peer        {}", component_name(&peer));
        written.push(peer);
    }
    #[cfg(windows)]
    {
        build_example_plugin(root)?;
    }
    Ok(written)
}

#[cfg(windows)]
fn stage_dispatcher(
    root: &Path,
    profile: &str,
    staged: &Path,
    version: &str,
    feature: &str,
    binary: &str,
    subsystem: &str,
) -> Result<PathBuf, String> {
    let online = !feature.is_empty();
    let build_profile = if profile == "installer" {
        "bootstrap"
    } else {
        profile
    };
    let mut args = vec![
        "build".to_owned(),
        "-p".to_owned(),
        DISPATCHER_PACKAGE.to_owned(),
        "--bin".to_owned(),
        (*binary).to_owned(),
        "--target".to_owned(),
        dispatcher::TARGET.rust_triple().to_owned(),
        "--profile".to_owned(),
        build_profile.to_owned(),
    ];
    if online {
        args.push("--features".to_owned());
        args.push((*feature).to_owned());
    }
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let built = target_directory(root, dispatcher::TARGET.rust_triple())
        .join(output_directory(build_profile))
        .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
    cargo(root, &args)?;
    let component = zup_toolchain::ToolchainComponent::Dispatcher {
        subsystem: match subsystem {
            "gui" => zup_toolchain::Subsystem::Gui,
            _ => zup_toolchain::Subsystem::Console,
        },
        online,
    };
    let staged_file = stage_component(&built, staged, &component, version)?;
    println!("  dispatch {subsystem:<8} {}", component_name(&staged_file));
    Ok(staged_file)
}

fn build_cross(root: &Path, profile: &str, target: &str) -> Result<Vec<PathBuf>, String> {
    let component_target =
        zup_core::Target::parse(target).map_err(|error| format!("`{target}`: {error}"))?;
    match component_target.operating_system() {
        zup_core::TargetOperatingSystem::Linux => {
            build_cross_linux(root, profile, &component_target)
        }
        zup_core::TargetOperatingSystem::Windows => {
            build_cross_windows(root, profile, &component_target)
        }
        _ => Err(format!(
            "cross toolchain builds support Windows and Linux targets; `{target}` is neither"
        )),
    }
}

fn build_cross_linux(
    root: &Path,
    profile: &str,
    target: &zup_core::Target,
) -> Result<Vec<PathBuf>, String> {
    if zup_platform::capabilities(target).is_none() {
        return Err(format!("unsupported Linux toolchain target `{target}`"));
    }
    let version = version()?;
    let staged = staging_directory(root, profile, &version);
    std::fs::create_dir_all(&staged).map_err(|error| format!("{}: {error}", staged.display()))?;
    let mut written = Vec::new();
    for (frontend, _) in frontends_for(target) {
        let binary = format!("zup-setup-{frontend}");
        zigbuild(
            root,
            &[
                "--target",
                target.rust_triple(),
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
        let built = target_directory(root, target.rust_triple())
            .join(output_directory(profile))
            .join(&binary);
        let component = zup_toolchain::ToolchainComponent::Runtime {
            target: *target,
            frontend: match frontend {
                "gui" => zup_core::Frontend::Gui,
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

fn build_cross_windows(
    root: &Path,
    profile: &str,
    target: &zup_core::Target,
) -> Result<Vec<PathBuf>, String> {
    if target.operating_system() != zup_core::TargetOperatingSystem::Windows {
        return Err(format!("`{target}` is not a Windows target"));
    }
    let version = version()?;
    let staged = staging_directory(root, profile, &version);
    std::fs::create_dir_all(&staged).map_err(|error| format!("{}: {error}", staged.display()))?;
    let mut written = Vec::new();
    for component in zup_toolchain::supported_components(target) {
        match component {
            zup_toolchain::ToolchainComponent::Runtime { frontend, .. } => {
                let feature = frontend.as_str();
                let binary = format!("zup-setup-{feature}");
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
                        &binary,
                        "--target",
                        target.rust_triple(),
                        "--profile",
                        profile,
                    ],
                )
                .map_err(|error| {
                    format!(
                        "{error}; building Windows runtimes off Windows needs a Windows target \
                         toolchain (for example `cargo-xwin`), or install a release's toolchain \
                         with `zup toolchain install <dir>` instead"
                    )
                })?;
                let built = target_directory(root, target.rust_triple())
                    .join(output_directory(profile))
                    .join(format!("{binary}.exe"));
                let staged_file = stage_component(&built, &staged, &component, &version)?;
                println!("  runtime  {feature:<8} {}", component_name(&staged_file));
                written.push(staged_file);
            }
            zup_toolchain::ToolchainComponent::Dispatcher { .. } => {}
            zup_toolchain::ToolchainComponent::Preset => {
                println!(
                    "  preset             skipped: a preset package executes a native binary to \
                     describe itself. Pack it on the target host (`zup preset pack --build \
                     <target>`), or pack binaries built elsewhere with `--describe-file \
                     <document>` from a native `--zup-describe` run"
                );
            }
        }
    }
    #[cfg(windows)]
    for (feature, binary, subsystem) in DISPATCHERS {
        written.push(stage_dispatcher(
            root, profile, &staged, &version, feature, binary, subsystem,
        )?);
    }
    #[cfg(not(windows))]
    {
        println!(
            "  dispatch           skipped: launchers link against Windows libraries; build them \
             on Windows or install a release's toolchain with `zup toolchain install <dir>`"
        );
    }
    Ok(written)
}

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

fn build_and_stage_preset(
    root: &Path,
    staged: &Path,
    version: &str,
    profile: &str,
) -> Result<PathBuf, String> {
    let scratch = tempfile::tempdir_in(staged)
        .map_err(|error| format!("a scratch directory in {}: {error}", staged.display()))?;
    let host = zup_core::Target::host().map_err(|error| error.to_string())?;
    let binary_name = "zup-preset-default";
    cargo(
        root,
        &[
            "build",
            "-p",
            PRESET_PACKAGE,
            "--bin",
            binary_name,
            "--profile",
            profile,
        ],
    )?;
    let built =
        target_directory(root, profile).join(format!("{binary_name}{}", host.executable_suffix()));
    if !built.is_file() {
        return Err(format!("the preset build wrote no `{}`", built.display()));
    }
    let describe = scratch.path().join("describe.json");
    let document = Command::new(&built)
        .arg("--zup-describe")
        .output()
        .map_err(|error| format!("describe the preset: {error}"))?;
    if !document.status.success() {
        return Err(format!(
            "the preset's describe mode failed: {}",
            String::from_utf8_lossy(&document.stderr).trim()
        ));
    }
    std::fs::write(&describe, &document.stdout)
        .map_err(|error| format!("{}: {error}", describe.display()))?;
    let mut packaged = built.clone();
    if !cfg!(windows) {
        let stripped = scratch.path().join(format!(
            "{binary_name}-stripped{}",
            host.executable_suffix()
        ));
        std::fs::copy(&built, &stripped)
            .map_err(|error| format!("{}: {error}", stripped.display()))?;
        match Command::new("strip").arg(&stripped).status() {
            Ok(status) if status.success() => packaged = stripped,
            _ => println!(
                "  preset             no `strip` for debug symbols; staging the unstripped binary"
            ),
        }
    }
    let binaries = [(host.as_str().to_owned(), packaged)];
    let package = scratch.path().join(zup_toolchain::PRESET_PACKAGE_NAME);
    let mut pack = vec![
        "run".to_owned(),
        "-p".to_owned(),
        CLI_PACKAGE.to_owned(),
        "--profile".to_owned(),
        "dev".to_owned(),
        "--".to_owned(),
        "preset".to_owned(),
        "pack".to_owned(),
        "--manifest".to_owned(),
        root.join(PRESET_DIRECTORY).to_string_lossy().into_owned(),
    ];
    for (target, path) in &binaries {
        pack.push("--binary".to_owned());
        pack.push(format!("{target}={}", path.to_string_lossy()));
    }
    pack.extend([
        "--describe-file".to_owned(),
        describe.to_string_lossy().into_owned(),
        "--output".to_owned(),
        package.to_string_lossy().into_owned(),
        "--force".to_owned(),
    ]);
    cargo(root, &pack.iter().map(String::as_str).collect::<Vec<_>>())?;
    stage_component(
        &package,
        staged,
        &zup_toolchain::ToolchainComponent::Preset,
        version,
    )
}

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

fn runtime_component(
    target: zup_core::Target,
    frontend: &str,
) -> zup_toolchain::ToolchainComponent {
    zup_toolchain::ToolchainComponent::Runtime {
        target,
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

pub fn assemble_sources(
    sources: &[PathBuf],
    version: &str,
    destination: &Path,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    use zup_artifact::preset::{PresetPackageView, PresetPackageWriter};

    if sources.is_empty() {
        return Err("toolchain assembly needs at least one source".into());
    }
    std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    if std::fs::read_dir(destination)
        .map_err(|error| error.to_string())?
        .next()
        .is_some()
    {
        return Err(format!(
            "assembly destination {} must be empty",
            destination.display()
        ));
    }
    let mut files = BTreeMap::new();
    let mut description = None;
    let mut binaries = BTreeMap::new();
    for source in sources {
        let directory = source.join(version);
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err(format!(
                    "expected a staged file at {}",
                    entry.path().display()
                ));
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or("non-UTF-8 staged file name")?;
            if name
                == format!(
                    "{}{}",
                    zup_toolchain::PRESET_PACKAGE_NAME,
                    zup_toolchain::DESCRIPTOR_SUFFIX
                )
            {
                continue;
            }
            if ["", ".exe"]
                .iter()
                .any(|suffix| name == zup_toolchain::test_preset_file_name(suffix))
            {
                continue;
            }
            if name == zup_toolchain::PRESET_PACKAGE_NAME {
                let descriptor_path =
                    directory.join(format!("{name}{}", zup_toolchain::DESCRIPTOR_SUFFIX));
                let descriptor = std::fs::read(&descriptor_path)
                    .map_err(|error| format!("{}: {error}", descriptor_path.display()))?;
                let descriptor = zup_toolchain::ComponentDescriptor::parse(&descriptor)
                    .map_err(|error| error.to_string())?;
                let expected = zup_toolchain::ComponentDescriptor::of(
                    &zup_toolchain::ToolchainComponent::Preset,
                    version,
                    &entry.path(),
                )
                .map_err(|error| error.to_string())?;
                if descriptor != expected {
                    return Err(format!(
                        "staged preset descriptor does not match {}",
                        entry.path().display()
                    ));
                }
                let bytes = std::fs::read(entry.path()).map_err(|error| error.to_string())?;
                let view = PresetPackageView::open(bytes).map_err(|error| error.to_string())?;
                view.verify().map_err(|error| error.to_string())?;
                let package = view.package();
                let current = zup_preset_protocol::PresetDescription {
                    name: package.name.clone(),
                    version: package.version.to_string(),
                    ui_protocol: package.ui_protocol,
                    required_capabilities: package.required_capabilities.clone(),
                    settings_schema: package.settings_schema.clone(),
                };
                if description
                    .as_ref()
                    .is_some_and(|previous| previous != &current)
                {
                    return Err("staged presets have different descriptions".into());
                }
                description = Some(current);
                for binary in &package.binaries {
                    let bytes = view
                        .binary_for(&binary.target)
                        .map_err(|error| error.to_string())?;
                    match binaries.entry(binary.target) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(bytes);
                        }
                        std::collections::btree_map::Entry::Occupied(entry)
                            if entry.get() != &bytes =>
                        {
                            return Err(format!("conflicting preset binary for {}", binary.target));
                        }
                        _ => {}
                    }
                }
            } else {
                if let Some(previous) = files.get(name) {
                    let fingerprint = |path: &PathBuf| {
                        std::fs::File::open(path)
                            .map_err(|error| error.to_string())
                            .and_then(|file| {
                                zup_core::hash_reader(file).map_err(|error| error.to_string())
                            })
                    };
                    if fingerprint(previous)? != fingerprint(&entry.path())? {
                        return Err(format!("conflicting staged component `{name}`"));
                    }
                } else {
                    files.insert(name.to_owned(), entry.path());
                }
            }
        }
    }
    for (name, source) in files {
        copy(&source, &destination.join(name))?;
    }
    if let Some(description) = description {
        let mut writer =
            PresetPackageWriter::new(description).map_err(|error| error.to_string())?;
        for (target, bytes) in binaries {
            writer
                .add_binary(target, bytes)
                .map_err(|error| error.to_string())?;
        }
        let scratch = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
        std::fs::write(
            scratch.path(),
            writer.finish().map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        stage_component(
            scratch.path(),
            destination,
            &zup_toolchain::ToolchainComponent::Preset,
            version,
        )?;
    }
    Ok(())
}

pub fn package(
    root: &Path,
    profile: &str,
    out: &Path,
    sources: &[PathBuf],
) -> Result<PathBuf, String> {
    let version = version()?;
    let assembly = tempfile::tempdir().map_err(|error| error.to_string())?;
    let staged = if sources.is_empty() {
        staging_directory(root, profile, &version)
    } else {
        assemble_sources(sources, &version, assembly.path())?;
        assembly.path().to_path_buf()
    };
    if !staged.is_dir() {
        return Err(format!(
            "there is no {} toolchain in {}; run `cargo xtask toolchain build --profile {profile}` first",
            profile,
            staged.display()
        ));
    }
    std::fs::create_dir_all(out).map_err(|error| format!("{}: {error}", out.display()))?;

    let cli_profile = if profile == "installer" {
        "release"
    } else {
        profile
    };
    cargo(
        root,
        &[
            "build",
            "-p",
            CLI_PACKAGE,
            "--bin",
            CLI_PACKAGE,
            "--profile",
            cli_profile,
        ],
    )?;
    let built =
        target_directory(root, cli_profile).join(format!("{CLI_PACKAGE}{EXECUTABLE_SUFFIX}"));
    copy(
        &built,
        &out.join(format!("{CLI_PACKAGE}{EXECUTABLE_SUFFIX}")),
    )?;

    let mut components = Vec::new();
    let test_peer = zup_toolchain::test_preset_file_name(EXECUTABLE_SUFFIX);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&staged)
        .map_err(|error| format!("{}: {error}", staged.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", staged.display()))?
        .into_iter()
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

    let mut index = zup_toolchain::ToolchainRelease::new(
        version.clone(),
        zup_core::Target::host()
            .map_err(|error| error.to_string())?
            .to_string(),
    );
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

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(to);
    std::fs::copy(from, to)
        .map_err(|error| format!("`{}` -> `{}`: {error}", from.display(), to.display()))?;
    Ok(())
}

fn cargo(root: &Path, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new(cargo_executable());
    command.current_dir(root);
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
        let listed = zup_toolchain::host_components(&zup_core::Target::host().unwrap());
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
    fn the_stager_produces_exactly_the_components_the_contract_names() {
        let target = zup_core::Target::host().unwrap();
        let wanted: Vec<String> = zup_toolchain::supported_components(&target)
            .iter()
            .map(|component| zup_toolchain::file_name(component, EXECUTABLE_SUFFIX))
            .collect();

        let mut staged: Vec<String> = frontends_for(&target)
            .iter()
            .map(|(feature, _)| {
                zup_toolchain::file_name(&runtime_component(target, feature), EXECUTABLE_SUFFIX)
            })
            .collect();
        staged.push(zup_toolchain::file_name(
            &zup_toolchain::ToolchainComponent::Preset,
            EXECUTABLE_SUFFIX,
        ));
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
        }
        staged.sort();
        let mut wanted = wanted;
        wanted.sort();
        assert_eq!(staged, wanted);
    }
}

#[cfg(test)]
mod assembly_tests {
    use super::*;

    #[test]
    fn assembly_merges_native_presets_and_regenerates_the_descriptor() {
        use zup_artifact::preset::{PresetPackageView, PresetPackageWriter};
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("assembled");
        std::fs::create_dir(&output).unwrap();
        let mut sources = Vec::new();
        for target in [zup_core::Target::WINDOWS_X64, zup_core::Target::LINUX_X64] {
            let source = root.path().join(target.as_str());
            std::fs::create_dir_all(source.join("0.0.1")).unwrap();
            let mut writer = PresetPackageWriter::new(zup_preset_protocol::PresetDescription::new(
                "default",
                "0.0.1",
                serde_json::json!({"type": "object"}),
            ))
            .unwrap();
            writer
                .add_binary(target, target.as_str().as_bytes().to_vec())
                .unwrap();
            std::fs::write(
                source
                    .join("0.0.1")
                    .join(zup_toolchain::PRESET_PACKAGE_NAME),
                writer.finish().unwrap(),
            )
            .unwrap();
            let path = source
                .join("0.0.1")
                .join(zup_toolchain::PRESET_PACKAGE_NAME);
            let descriptor = zup_toolchain::ComponentDescriptor::of(
                &zup_toolchain::ToolchainComponent::Preset,
                "0.0.1",
                &path,
            )
            .unwrap();
            std::fs::write(
                source.join("0.0.1").join(format!(
                    "{}{}",
                    zup_toolchain::PRESET_PACKAGE_NAME,
                    zup_toolchain::DESCRIPTOR_SUFFIX
                )),
                descriptor.encode(),
            )
            .unwrap();
            sources.push(source);
        }
        assemble_sources(&sources, "0.0.1", &output).unwrap();
        let path = output.join(zup_toolchain::PRESET_PACKAGE_NAME);
        let view = PresetPackageView::open(std::fs::read(&path).unwrap()).unwrap();
        view.verify().unwrap();
        assert_eq!(view.targets().len(), 2);
        for target in [zup_core::Target::WINDOWS_X64, zup_core::Target::LINUX_X64] {
            assert_eq!(
                view.binary_for(&target).unwrap(),
                target.as_str().as_bytes()
            );
        }
        let expected = zup_toolchain::ComponentDescriptor::of(
            &zup_toolchain::ToolchainComponent::Preset,
            "0.0.1",
            &path,
        )
        .unwrap();
        let descriptor = std::fs::read(output.join(format!(
            "{}{}",
            zup_toolchain::PRESET_PACKAGE_NAME,
            zup_toolchain::DESCRIPTOR_SUFFIX
        )))
        .unwrap();
        assert_eq!(
            zup_toolchain::ComponentDescriptor::parse(&descriptor).unwrap(),
            expected
        );
    }

    #[test]
    fn downloaded_toolchains_keep_their_version_and_conflicts_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let windows = root.path().join("windows");
        let linux = root.path().join("linux");
        let output = root.path().join("assembled");
        for directory in [&windows, &linux] {
            std::fs::create_dir_all(directory.join("0.0.1")).unwrap();
        }
        std::fs::create_dir(&output).unwrap();
        std::fs::write(windows.join("0.0.1/windows-runtime"), b"windows").unwrap();
        std::fs::write(linux.join("0.0.1/linux-runtime"), b"linux").unwrap();
        let sources = [windows.clone(), linux.clone()];
        assemble_sources(&sources, "0.0.1", &output).unwrap();
        assert_eq!(
            std::fs::read(output.join("windows-runtime")).unwrap(),
            b"windows"
        );
        assert_eq!(
            std::fs::read(output.join("linux-runtime")).unwrap(),
            b"linux"
        );
        std::fs::remove_dir_all(&output).unwrap();
        assert!(assemble_sources(&sources, "0.0.2", &output).is_err());
        std::fs::write(linux.join("0.0.1/windows-runtime"), b"different").unwrap();
        assert!(assemble_sources(&sources, "0.0.1", &output).is_err());
    }
}
