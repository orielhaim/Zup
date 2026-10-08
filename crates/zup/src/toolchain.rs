use std::path::{Path, PathBuf};

use zup_core::Frontend;
use zup_toolchain::{ComponentDescriptor, Subsystem, ToolchainComponent, ToolchainError};

pub const TOOLCHAIN_ENV: &str = "ZUP_TOOLCHAIN";

fn suffix_for(component: &ToolchainComponent) -> &'static str {
    match component {
        ToolchainComponent::Runtime { target, .. } => target.executable_suffix(),
        ToolchainComponent::Dispatcher { .. } => ".exe",
        ToolchainComponent::Preset => "",
    }
}

fn file_name_for(component: &ToolchainComponent) -> String {
    zup_toolchain::file_name(component, suffix_for(component))
}

pub const STAGED_DIRECTORY: &str = "toolchain";

pub const CACHE_DIRECTORY: &str = "toolchain";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolchainSource {
    Override,
    Root,
    Cache,
    Staged,
}

impl ToolchainSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Root => "toolchain root",
            Self::Cache => "cache",
            Self::Staged => "staged",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedComponent {
    pub path: PathBuf,
    pub source: ToolchainSource,
    pub descriptor: ComponentDescriptor,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no zup {kind} for {wanted} was found in {searched}")]
    NotFound {
        kind: &'static str,
        wanted: String,
        searched: String,
    },
    #[error(transparent)]
    Unusable(#[from] ToolchainError),
}

pub struct ToolchainResolver {
    version: String,
    override_root: Option<PathBuf>,
    executable: PathBuf,
    state_root: PathBuf,
}

impl ToolchainResolver {
    pub fn new(version: String, executable: PathBuf, state_root: PathBuf) -> Self {
        Self {
            version,
            override_root: std::env::var_os(TOOLCHAIN_ENV).map(PathBuf::from),
            executable,
            state_root,
        }
    }

    pub fn with_root(mut self, root: Option<PathBuf>) -> Self {
        if root.is_some() {
            self.override_root = root;
        }
        self
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn resolve(
        &self,
        component: &ToolchainComponent,
        explicit: Option<&Path>,
    ) -> Result<ResolvedComponent, ResolveError> {
        if let Some(path) = explicit {
            let path = absolute(path);
            return Ok(ResolvedComponent {
                descriptor: check(&path, component, &self.version)?,
                path,
                source: ToolchainSource::Override,
            });
        }
        let mut searched = Vec::new();
        for (root, source) in self.roots() {
            let path = root.join(file_name_for(component));
            searched.push(path.display().to_string());
            if !path.is_file() {
                continue;
            }
            let descriptor = check(&path, component, &self.version)?;
            return Ok(ResolvedComponent {
                path,
                source,
                descriptor,
            });
        }
        Err(ResolveError::NotFound {
            kind: match component {
                ToolchainComponent::Runtime { .. } => "runtime template",
                ToolchainComponent::Dispatcher { .. } => "dispatcher",
                ToolchainComponent::Preset => "preset package",
            },
            wanted: describe(component),
            searched: searched.join("\n  "),
        })
    }

    pub fn roots(&self) -> Vec<(PathBuf, ToolchainSource)> {
        let mut candidates = Vec::new();
        if let Some(root) = &self.override_root {
            candidates.push((root.clone(), ToolchainSource::Root));
        }
        let cache = self.state_root.join(CACHE_DIRECTORY).join(&self.version);
        candidates.push((cache, ToolchainSource::Cache));

        let mut beside = self.executable.clone();
        beside.pop();
        if beside.ends_with("deps") {
            beside.pop();
        }
        candidates.push((
            beside.join(STAGED_DIRECTORY).join(&self.version),
            ToolchainSource::Staged,
        ));
        candidates.push((beside.join(STAGED_DIRECTORY), ToolchainSource::Staged));
        candidates
    }
}

fn check(
    path: &Path,
    component: &ToolchainComponent,
    version: &str,
) -> Result<ComponentDescriptor, ToolchainError> {
    let descriptor = zup_toolchain::read(path, component, version)?;
    let executable = || zup_binary::Executable::read(path);
    let unreadable = |error: zup_binary::InspectError| ToolchainError::Unreadable {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };
    match component {
        ToolchainComponent::Runtime { target, frontend } => {
            let runtime = executable().map_err(unreadable)?;
            if !runtime.matches_target(target) {
                return Err(ToolchainError::WrongTarget {
                    found: runtime
                        .architectures()
                        .iter()
                        .map(|machine| machine.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    wanted: target.as_str().to_owned(),
                });
            }
            if !runtime.matches_frontend(*frontend) {
                return Err(ToolchainError::WrongFrontend {
                    found: format!("{:?}", runtime.program()).to_lowercase(),
                    wanted: frontend.as_str().to_owned(),
                });
            }
        }
        ToolchainComponent::Dispatcher { subsystem, .. } => {
            let launcher = executable().map_err(unreadable)?;
            let found = match launcher.program() {
                Some(zup_binary::ProgramKind::Windowed) => zup_core::Frontend::Gui,
                Some(zup_binary::ProgramKind::Console) => zup_core::Frontend::Console,
                None => {
                    return Err(ToolchainError::Unreadable {
                        path: path.to_path_buf(),
                        reason: "the file records no window/terminal subsystem".to_owned(),
                    });
                }
            };
            if !subsystem.carries(found) {
                return Err(ToolchainError::WrongSubsystem {
                    found: found.as_str().to_owned(),
                    wanted: subsystem.as_str().to_owned(),
                });
            }
        }
        ToolchainComponent::Preset => {
            let bytes = std::fs::read(path).map_err(|error| ToolchainError::Unreadable {
                path: path.to_path_buf(),
                reason: error.to_string(),
            })?;
            zup_artifact::preset::PresetPackageView::open(bytes)
                .and_then(|view| view.verify())
                .map_err(|error| ToolchainError::Unreadable {
                    path: path.to_path_buf(),
                    reason: error.to_string(),
                })?;
        }
    }
    Ok(descriptor)
}

pub fn runtime_for(target: &zup_core::TargetTriple, frontend: Frontend) -> ToolchainComponent {
    ToolchainComponent::Runtime {
        target: target.clone(),
        frontend,
    }
}

pub fn cache_directory(state_root: &Path, version: &str) -> PathBuf {
    state_root.join(CACHE_DIRECTORY).join(version)
}

pub fn other_cached_versions(state_root: &Path, keep: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(state_root.join(CACHE_DIRECTORY)) else {
        return Vec::new();
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name != keep)
        .collect();
    versions.sort();
    versions
}

pub fn other_cached_versions_for_self(state_root: &Path) -> Vec<String> {
    other_cached_versions(state_root, crate::ZUP_VERSION)
}

pub fn dispatcher_for(
    subsystem: zup_artifact::LauncherSubsystem,
    online: bool,
) -> ToolchainComponent {
    ToolchainComponent::Dispatcher {
        subsystem: match subsystem {
            zup_artifact::LauncherSubsystem::Gui => Subsystem::Gui,
            zup_artifact::LauncherSubsystem::Console => Subsystem::Console,
        },
        online,
    }
}

pub fn describe(component: &ToolchainComponent) -> String {
    match component {
        ToolchainComponent::Runtime { target, frontend } => {
            format!("{} runtime for {}", frontend.as_str(), target.as_str())
        }
        ToolchainComponent::Dispatcher { subsystem, .. } => {
            format!("{} dispatcher", subsystem.as_str())
        }
        ToolchainComponent::Preset => "preset package".to_owned(),
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub fn missing_component_message(component: &ToolchainComponent, error: &ResolveError) -> String {
    let kind = match component {
        ToolchainComponent::Runtime { .. } => "runtime template",
        ToolchainComponent::Dispatcher { .. } => "dispatcher",
        ToolchainComponent::Preset => "preset package",
    };
    format!(
        "no {kind} for {} was found.\n\n  \
         Build the zup toolchain for this version and stage it, or point zup at one:\n    \
         cargo xtask toolchain build\n    \
         zup --toolchain <dir> ...   (a directory of components)\n\n  {}",
        describe(component),
        error
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_precedence_is_root_then_cache_then_staged() {
        let root = PathBuf::from("C:/toolchain");
        let state = PathBuf::from("C:/state");
        let executable = PathBuf::from("C:/bin/zup.exe");
        let resolver = ToolchainResolver::new("0.1.0".into(), executable.clone(), state.clone())
            .with_root(Some(root.clone()));
        let sources: Vec<ToolchainSource> =
            resolver.roots().iter().map(|(_, source)| *source).collect();
        assert_eq!(
            sources,
            vec![
                ToolchainSource::Root,
                ToolchainSource::Cache,
                ToolchainSource::Staged,
                ToolchainSource::Staged
            ],
            "an explicit root wins, then the pinned cache, then a staged toolchain"
        );
        assert_eq!(resolver.roots()[0].0, root);
        assert_eq!(
            resolver.roots()[1].0,
            state.join("toolchain").join("0.1.0"),
            "the cache is keyed by the exact zup version, so a pinned build is reproducible"
        );
    }

    #[test]
    fn a_component_is_stored_under_its_own_target_suffix() {
        let linux = runtime_for(
            &zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").expect("valid"),
            Frontend::Console,
        );
        let name = file_name_for(&linux);
        assert!(
            name == "zup-setup-console-x86_64-unknown-linux-gnu",
            "a Linux template is extensionless even when resolved on a Windows host: {name}"
        );
        let windows = runtime_for(
            &zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid"),
            Frontend::Console,
        );
        assert_eq!(
            file_name_for(&windows),
            "zup-setup-console-x86_64-pc-windows-msvc.exe"
        );
    }

    #[test]
    fn a_missing_component_refusal_names_how_to_produce_one() {
        let component = runtime_for(
            &zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid"),
            Frontend::Gui,
        );
        let error = ResolveError::NotFound {
            kind: "runtime template",
            wanted: describe(&component),
            searched: "C:/state/toolchain/0.1.0/zup-setup-gui-x86_64-pc-windows-msvc.exe"
                .to_owned(),
        };
        let message = missing_component_message(&component, &error);
        assert!(message.contains("cargo xtask toolchain build"), "{message}");
        assert!(message.contains("--toolchain"), "{message}");
        assert!(message.contains("runtime"), "{message}");
        assert!(
            !message.contains("https://"),
            "an offline build must not be told to fetch from the network"
        );
    }

    /// cannot find a component inside one, so a person's build never quietly
    #[test]
    fn a_resolver_outside_a_checkout_only_ever_looks_in_the_two_places_it_was_given() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the repository root")
            .to_path_buf();
        let outside = std::env::temp_dir().join("zup-not-in-a-checkout");
        let resolver = ToolchainResolver::new(
            "0.1.0".into(),
            outside.join("bin").join("zup.exe"),
            outside.join("state"),
        );
        assert_eq!(
            resolver.roots(),
            vec![
                (
                    outside.join("state").join(CACHE_DIRECTORY).join("0.1.0"),
                    ToolchainSource::Cache
                ),
                (
                    outside.join("bin").join(STAGED_DIRECTORY).join("0.1.0"),
                    ToolchainSource::Staged
                ),
                (
                    outside.join("bin").join(STAGED_DIRECTORY),
                    ToolchainSource::Staged
                ),
            ],
            "a pinned cache, then a staged directory beside the executable"
        );
        for (root, _) in resolver.roots() {
            assert!(
                !root.starts_with(&repository),
                "{} is inside the checkout at {}",
                root.display(),
                repository.display()
            );
        }
        let current = std::env::current_dir().expect("a working directory");
        assert!(
            !resolver
                .roots()
                .iter()
                .any(|(root, _)| root.starts_with(&current)),
            "resolution must not depend on where the process is standing"
        );

        let deps = ToolchainResolver::new(
            "0.1.0".into(),
            PathBuf::from("C:/target/debug/deps/zup.exe"),
            PathBuf::from("C:/state"),
        );
        assert_eq!(
            deps.roots().last().expect("a staged candidate").0,
            PathBuf::from("C:/target/debug/toolchain")
        );
    }
}

use std::fmt::Write as _;

use clap::{Args, ValueHint};
use zup_automation::{
    AutomationResult, Details, Identifier, LogLevel, ToolchainCleanDetails,
    ToolchainInstallDetails, ToolchainStatusDetails,
};
use zup_toolchain::ToolchainRelease;

use crate::cli::OutputArg;
use crate::failure::Reporter;

#[derive(Debug, Args)]
pub struct ToolchainCommand {
    #[command(subcommand)]
    pub command: ToolchainVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum ToolchainVerb {
    Install(ToolchainInstallCommand),
    Status(ToolchainStatusCommand),
    Clean(ToolchainCleanCommand),
}

#[derive(Debug, Args)]
pub struct ToolchainInstallCommand {
    #[arg(value_name = "RELEASE", value_hint = ValueHint::DirPath)]
    pub source: PathBuf,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

#[derive(Debug, Args)]
pub struct ToolchainStatusCommand {
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

#[derive(Debug, Args)]
pub struct ToolchainCleanCommand {
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ComponentStatus {
    pub component: String,
    pub found: bool,
    pub source: Option<ToolchainSourceName>,
    pub path: Option<String>,
    pub problem: Option<String>,
}

impl ToolchainSourceName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Root => "root",
            Self::Cache => "cache",
            Self::Staged => "staged",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolchainSourceName {
    Override,
    Root,
    Cache,
    Staged,
}

impl From<ToolchainSource> for ToolchainSourceName {
    fn from(value: ToolchainSource) -> Self {
        match value {
            ToolchainSource::Override => Self::Override,
            ToolchainSource::Root => Self::Root,
            ToolchainSource::Cache => Self::Cache,
            ToolchainSource::Staged => Self::Staged,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolchainStatus {
    pub zup_version: String,
    pub host: String,
    pub cache: String,
    pub cache_populated: bool,
    pub other_versions: Vec<String>,
    pub components: Vec<ComponentStatus>,
}

impl ToolchainStatus {
    pub fn is_complete(&self) -> bool {
        self.components.iter().all(|component| component.found)
    }

    pub fn human(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "zup {}  on  {}", self.zup_version, self.host);
        let _ = writeln!(out, "  cache    {}", self.cache);
        let _ = writeln!(
            out,
            "  state    {}",
            if self.cache_populated {
                "populated"
            } else {
                "empty"
            }
        );
        if !self.other_versions.is_empty() {
            let _ = writeln!(
                out,
                "  also     {} (removed by `zup toolchain clean`)",
                self.other_versions.join(", ")
            );
        }
        out.push_str("\nComponents\n");
        for component in &self.components {
            let _ = writeln!(
                out,
                "  {} {:<34} {}",
                if component.found { "✓" } else { "✗" },
                component.component,
                component
                    .source
                    .map(|source| format!("({:?})", source).to_lowercase())
                    .unwrap_or_default()
            );
            if let Some(path) = &component.path {
                let _ = writeln!(out, "    {path}");
            }
            if let Some(problem) = &component.problem {
                let _ = writeln!(out, "    {problem}");
            }
        }
        let _ = writeln!(
            out,
            "\n{}",
            if self.is_complete() {
                "ready: every component a build needs is present and verified".to_owned()
            } else {
                let missing = self
                    .components
                    .iter()
                    .filter(|component| !component.found)
                    .count();
                format!(
                    "not ready: {missing} of {} component(s) missing",
                    self.components.len()
                )
            }
        );
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolchainCommandError {
    #[error("`{path}` is not a zup release: {reason}")]
    NotARelease { path: PathBuf, reason: String },
    #[error("the release at `{path}` is not intact: {reason}")]
    Damaged { path: PathBuf, reason: String },
    #[error(
        "the release at `{path}` is zup {found} and this is zup {version}; a toolchain is only \
         usable by the release that produced it"
    )]
    WrongVersion {
        path: PathBuf,
        found: String,
        version: String,
    },
    #[error("the cache for zup {version} is not usable:\n  {}", problems.join("\n  "))]
    Incomplete {
        version: String,
        problems: Vec<String>,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn state_root(named: Option<PathBuf>) -> miette::Result<PathBuf> {
    match named {
        Some(root) => Ok(root),
        None => crate::toolchain_state_root(),
    }
}

pub fn run_install(
    args: ToolchainInstallCommand,
    toolchain_root: Option<PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let source = args.source.canonicalize().map_err(|source| {
        crate::failure::error(
            "zup.toolchain.source_unreadable",
            format!("{}: {source}", args.source.display()),
        )
    })?;
    let state_root = state_root(args.state_root.clone())?;
    let installed = install(&source, &state_root, toolchain_root).map_err(failure)?;
    reporter.log(LogLevel::Info, installed.human());
    let count = installed.components.len();
    Ok(
        AutomationResult::new(zup_automation::OPERATION_TOOLCHAIN_INSTALL)
            .with_details(Details::ToolchainInstall(ToolchainInstallDetails {
                zup_version: installed.zup_version.clone(),
                source: crate::automation::project_path(std::path::Path::new(&installed.source)),
                cache: installed.cache.clone(),
                components: installed.components.clone(),
            }))
            .with_summary(format!(
                "Installed {count} toolchain component(s) for zup {}",
                installed.zup_version
            )),
    )
}

fn failure(error: ToolchainCommandError) -> miette::Report {
    use ToolchainCommandError as Error;
    match &error {
        Error::NotARelease { .. } => {
            crate::failure::error("zup.toolchain.not_a_release", error.to_string())
        }
        Error::Damaged { .. } => {
            crate::failure::error("zup.toolchain.release_damaged", error.to_string())
        }
        Error::WrongVersion { .. } => crate::failure::error_with_help(
            "zup.toolchain.wrong_version",
            error.to_string(),
            "A toolchain is usable only by the zup release that produced it.",
        ),
        Error::Incomplete { .. } => crate::failure::error_with_help(
            "zup.toolchain.cache_incomplete",
            error.to_string(),
            crate::doctor::TOOLCHAIN_HINT,
        ),
        Error::Io { .. } => crate::failure::error("zup.toolchain.io", error.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub zup_version: String,
    pub source: String,
    pub cache: String,
    pub components: Vec<String>,
}

impl Installed {
    fn human(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Installed the zup {} toolchain", self.zup_version);
        let _ = writeln!(out, "  from    {}", self.source);
        let _ = writeln!(out, "  into    {}", self.cache);
        for component in &self.components {
            let _ = writeln!(out, "  {component}");
        }
        out.push_str("\n`zup build` will find them without being told where they are.");
        out
    }
}

pub fn install(
    source: &Path,
    state_root: &Path,
    toolchain_root: Option<PathBuf>,
) -> Result<Installed, ToolchainCommandError> {
    let index = ToolchainRelease::read(source).map_err(|error| match error {
        zup_toolchain::ToolchainError::Unreadable { path, reason } => {
            ToolchainCommandError::NotARelease { path, reason }
        }
        other => ToolchainCommandError::NotARelease {
            path: source.to_path_buf(),
            reason: other.to_string(),
        },
    })?;
    if index.zup_version != crate::ZUP_VERSION {
        return Err(ToolchainCommandError::WrongVersion {
            path: source.to_path_buf(),
            found: index.zup_version,
            version: crate::ZUP_VERSION.to_owned(),
        });
    }
    index
        .verify(source)
        .map_err(|error| ToolchainCommandError::Damaged {
            path: source.to_path_buf(),
            reason: error.to_string(),
        })?;

    let cache = crate::toolchain::cache_directory(state_root, &index.zup_version);
    std::fs::create_dir_all(&cache).map_err(|error| ToolchainCommandError::Io {
        path: cache.clone(),
        source: error,
    })?;

    let mut components = Vec::new();
    for file in &index.components {
        let name = file
            .path
            .rsplit('/')
            .next()
            .ok_or_else(|| ToolchainCommandError::Damaged {
                path: source.to_path_buf(),
                reason: format!("`{}` is not a file path", file.path),
            })?
            .to_owned();
        let from = zup_toolchain::resolve(source, &file.path);
        let to = cache.join(&name);
        copy_component(&from, &to)?;
        if !name.ends_with(zup_toolchain::DESCRIPTOR_SUFFIX) {
            components.push(name);
        }
    }
    components.sort();

    let verifier = ToolchainResolver::new(
        crate::ZUP_VERSION.to_owned(),
        cache.join("zup.exe"),
        state_root.to_path_buf(),
    )
    .with_root(toolchain_root);
    let wanted = host_components();
    let mut missing = Vec::new();
    for component in &wanted {
        if let Err(error) = verifier.resolve(component, None) {
            missing.push(format!("{}: {error}", describe(component)));
        }
    }
    if !missing.is_empty() {
        return Err(ToolchainCommandError::Incomplete {
            version: crate::ZUP_VERSION.to_owned(),
            problems: missing,
        });
    }

    Ok(Installed {
        zup_version: index.zup_version,
        source: crate::plain_path(source),
        cache: crate::plain_path(&cache),
        components,
    })
}

/// either the previous component or the new one and never a half-written
fn copy_component(from: &Path, to: &Path) -> Result<(), ToolchainCommandError> {
    let temporary = to.with_extension("zup-installing");
    let _ = std::fs::remove_file(&temporary);
    let result = (|| {
        std::fs::copy(from, &temporary).map_err(|source| ToolchainCommandError::Io {
            path: from.to_path_buf(),
            source,
        })?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&temporary)
            .map_err(|source| ToolchainCommandError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.sync_all()
            .map_err(|source| ToolchainCommandError::Io {
                path: temporary.clone(),
                source,
            })?;
        std::fs::rename(&temporary, to).map_err(|source| ToolchainCommandError::Io {
            path: to.to_path_buf(),
            source,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub fn run_status(
    args: ToolchainStatusCommand,
    toolchain_root: Option<PathBuf>,
) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let report = status(&state_root(args.state_root.clone())?, toolchain_root);
    reporter.log(LogLevel::Info, report.human());
    let missing = report
        .components
        .iter()
        .filter(|component| !component.found)
        .count();
    let mut result = AutomationResult::new(zup_automation::OPERATION_TOOLCHAIN_STATUS)
        .with_details(Details::ToolchainStatus(ToolchainStatusDetails {
            zup_version: report.zup_version.clone(),
            host: report.host.clone(),
            cache: report.cache.clone(),
            complete: report.is_complete(),
            components: report
                .components
                .iter()
                .map(|component| zup_automation::ToolchainComponentStatus {
                    component: component.component.clone(),
                    found: component.found,
                    source: component
                        .source
                        .map(|source| Identifier::fixed(source.as_str())),
                    path: component.path.clone(),
                    problem: component.problem.clone(),
                })
                .collect(),
        }))
        .with_summary(if report.is_complete() {
            format!(
                "every toolchain component is present and verified ({})",
                crate::ZUP_VERSION
            )
        } else {
            format!(
                "not ready: {missing} of {} component(s) missing",
                report.components.len()
            )
        });
    if !report.is_complete() {
        result = result.failed().with_diagnostic(
            zup_automation::Diagnostic::error(
                "zup.toolchain.component_missing",
                format!(
                    "{missing} of {} component(s) a build needs are missing",
                    report.components.len()
                ),
            )
            .with_help(crate::doctor::TOOLCHAIN_HINT),
        );
    }
    Ok(result)
}

pub fn status(state_root: &Path, toolchain_root: Option<PathBuf>) -> ToolchainStatus {
    report(
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zup")),
        state_root,
        toolchain_root,
    )
}

fn report(
    executable: PathBuf,
    state_root: &Path,
    toolchain_root: Option<PathBuf>,
) -> ToolchainStatus {
    let resolver = ToolchainResolver::new(
        crate::ZUP_VERSION.to_owned(),
        executable,
        state_root.to_path_buf(),
    )
    .with_root(toolchain_root);
    let cache = crate::toolchain::cache_directory(state_root, crate::ZUP_VERSION);
    let components = host_components()
        .into_iter()
        .map(|component| match resolver.resolve(&component, None) {
            Ok(found) => ComponentStatus {
                component: describe(&component),
                found: true,
                source: Some(found.source.into()),
                path: Some(crate::plain_path(&found.path)),
                problem: None,
            },
            Err(error) => ComponentStatus {
                component: describe(&component),
                found: false,
                source: None,
                path: None,
                problem: Some(error.to_string()),
            },
        })
        .collect();
    ToolchainStatus {
        zup_version: crate::ZUP_VERSION.to_owned(),
        host: zup_plugin_contract::HOST_TARGET.to_owned(),
        cache: crate::plain_path(&cache),
        cache_populated: cache.is_dir(),
        other_versions: crate::toolchain::other_cached_versions_for_self(state_root),
        components,
    }
}

fn host_components() -> Vec<ToolchainComponent> {
    let target = zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET)
        .expect("the build host's own target triple is valid");
    zup_toolchain::supported_components(&target)
}

pub fn run_clean(args: ToolchainCleanCommand) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let state_root = state_root(args.state_root.clone())?;
    let report = clean(&state_root, args.all, args.dry_run).map_err(failure)?;
    reporter.log(LogLevel::Info, report.human());
    let removed = report.removed.len();
    Ok(
        AutomationResult::new(zup_automation::OPERATION_TOOLCHAIN_CLEAN)
            .with_details(Details::ToolchainClean(ToolchainCleanDetails {
                cache: report.cache.clone(),
                dry_run: report.dry_run,
                removed: report.removed.clone(),
                kept: report.kept.clone(),
            }))
            .with_summary(format!(
                "{} {removed} cached version(s) from {}",
                if report.dry_run {
                    "Would remove"
                } else {
                    "Removed"
                },
                report.cache
            )),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    pub cache: String,
    pub dry_run: bool,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
}

impl Cleaned {
    pub fn human(&self) -> String {
        let verb = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        let mut out = String::new();
        let _ = writeln!(out, "{verb} from {}", self.cache);
        for entry in &self.removed {
            let _ = writeln!(out, "  {entry}");
        }
        if self.removed.is_empty() {
            out.push_str("  nothing to remove\n");
        }
        for version in &self.kept {
            let _ = writeln!(out, "  kept    zup {version}");
        }
        out
    }
}

pub fn clean(
    state_root: &Path,
    all: bool,
    dry_run: bool,
) -> Result<Cleaned, ToolchainCommandError> {
    let cache_root = state_root.join(crate::toolchain::CACHE_DIRECTORY);
    let mut removed = Vec::new();
    for version in crate::toolchain::other_cached_versions_for_self(state_root) {
        let directory = cache_root.join(&version);
        let note = if dry_run {
            format!(
                "{version} ({})",
                zup_presentation::format_bytes(directory_size(&directory))
            )
        } else {
            std::fs::remove_dir_all(&directory).map_err(|error| ToolchainCommandError::Io {
                path: directory.clone(),
                source: error,
            })?;
            version
        };
        removed.push(note);
    }
    for (name, directory) in stray_files(&cache_root) {
        if !dry_run {
            std::fs::remove_file(&directory).map_err(|error| ToolchainCommandError::Io {
                path: directory.clone(),
                source: error,
            })?;
        }
        removed.push(name);
    }
    let mut kept = Vec::new();
    let current = crate::toolchain::cache_directory(state_root, crate::ZUP_VERSION);
    if all {
        if dry_run {
            if current.is_dir() {
                removed.push(crate::ZUP_VERSION.to_owned());
            }
        } else {
            match std::fs::remove_dir_all(&current) {
                Ok(()) => removed.push(crate::ZUP_VERSION.to_owned()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ToolchainCommandError::Io {
                        path: current,
                        source,
                    });
                }
            }
        }
    } else {
        kept.push(crate::ZUP_VERSION.to_owned());
    }
    Ok(Cleaned {
        cache: crate::plain_path(&cache_root),
        dry_run,
        removed,
        kept,
    })
}

/// unreachable by the resolver, so removing them is never wrong.
fn stray_files(cache_root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    out.sort();
    out
}

fn directory_size(directory: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok().map(|meta| meta.len()))
        .sum()
}

#[cfg(test)]
mod toolchain_cli_tests {
    use super::*;

    fn image(component: &ToolchainComponent) -> Vec<u8> {
        if let ToolchainComponent::Runtime { target, .. } = component
            && target.operating_system() == zup_core::TargetOperatingSystem::Linux
        {
            let machine: u16 = if target.as_str().starts_with("aarch64") {
                183
            } else {
                62
            };
            let mut bytes = vec![0u8; 64];
            bytes[..4].copy_from_slice(b"\x7fELF");
            bytes[4] = 2;
            bytes[5] = 1;
            bytes[6] = 1;
            bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
            bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
            bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
            return bytes;
        }
        if let ToolchainComponent::Preset = component {
            let mut writer = zup_artifact::preset::PresetPackageWriter::new(
                zup_preset_protocol::PresetDescription::new(
                    "aurora",
                    "1.0.0",
                    serde_json::json!({ "type": "object" }),
                ),
            )
            .expect("a valid description");
            writer
                .add_binary(
                    zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET)
                        .expect("a valid target"),
                    b"a preset".to_vec(),
                )
                .expect("one binary for the host");
            return writer.finish().expect("a verified package");
        }
        let (machine, subsystem): (u16, u16) = match component {
            ToolchainComponent::Runtime { target, frontend } => (
                if target.as_str().starts_with("aarch64") {
                    0xaa64
                } else {
                    0x8664
                },
                match frontend {
                    zup_core::Frontend::Gui => 2,
                    zup_core::Frontend::Console | zup_core::Frontend::Headless => 3,
                },
            ),
            ToolchainComponent::Dispatcher { subsystem, .. } => (
                0x014c,
                if *subsystem == zup_toolchain::Subsystem::Gui {
                    2
                } else {
                    3
                },
            ),
            ToolchainComponent::Preset => unreachable!("a package is not a PE image"),
        };
        let mut bytes = vec![0u8; 0x178];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[0x9c..0x9e].copy_from_slice(&subsystem.to_le_bytes());
        bytes
    }

    fn release(root: &Path) -> PathBuf {
        let staged = root.join(format!("toolchain/{}", crate::ZUP_VERSION));
        std::fs::create_dir_all(&staged).expect("the staged directory");
        let target = zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET)
            .expect("a valid target");
        let mut index = ToolchainRelease::new(crate::ZUP_VERSION, target.as_str());
        let cli_name = format!("zup{}", std::env::consts::EXE_SUFFIX);
        let cli = root.join(&cli_name);
        std::fs::write(&cli, b"a cli").expect("write the cli");
        index.cli = zup_toolchain::ReleaseFile::of(&cli_name, &cli).expect("measure the cli");
        for component in zup_toolchain::supported_components(&target) {
            let name = zup_toolchain::file_name(&component, component_suffix(&component));
            let path = staged.join(&name);
            std::fs::write(&path, image(&component)).expect("write a component");
            let descriptor =
                zup_toolchain::ComponentDescriptor::of(&component, crate::ZUP_VERSION, &path)
                    .expect("describe a component");
            let descriptor_name = format!("{name}{}", zup_toolchain::DESCRIPTOR_SUFFIX);
            let descriptor_path = staged.join(&descriptor_name);
            std::fs::write(&descriptor_path, descriptor.encode()).expect("write a descriptor");
            let relative = format!("toolchain/{}/{name}", crate::ZUP_VERSION);
            index
                .components
                .push(zup_toolchain::ReleaseFile::of(&relative, &path).expect("measure"));
            index.components.push(
                zup_toolchain::ReleaseFile::of(
                    &format!("{relative}{}", zup_toolchain::DESCRIPTOR_SUFFIX),
                    &descriptor_path,
                )
                .expect("measure"),
            );
        }
        index
            .components
            .sort_by(|left, right| left.path.cmp(&right.path));
        std::fs::write(root.join(zup_toolchain::RELEASE_INDEX_NAME), index.encode())
            .expect("write the index");
        root.to_path_buf()
    }

    /// report built from `current_exe` would find that and never exercise the
    fn isolated_status(state: &Path) -> ToolchainStatus {
        report(state.join("no-such-toolchain").join("zup.exe"), state, None)
    }

    fn file_name_of(path: &str) -> String {
        path.rsplit('/').next().expect("a file name").to_owned()
    }

    fn component_suffix(component: &ToolchainComponent) -> &'static str {
        match component {
            ToolchainComponent::Runtime { target, .. } => target.executable_suffix(),
            ToolchainComponent::Dispatcher { .. } => ".exe",
            ToolchainComponent::Preset => "",
        }
    }

    #[test]
    fn an_installed_toolchain_beats_a_staged_one_and_an_explicit_root() {
        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");
        install(&release(&directory.path().join("material")), &state, None).expect("install");

        let executable = directory.path().join("bin").join("zup.exe");
        std::fs::create_dir_all(executable.parent().expect("a parent")).expect("bin");
        std::fs::write(&executable, b"an executable").expect("write it");
        let staged = executable
            .parent()
            .expect("a parent")
            .join(crate::toolchain::STAGED_DIRECTORY)
            .join(crate::ZUP_VERSION);
        std::fs::create_dir_all(&staged).expect("the staged directory");
        for component in host_components() {
            let name = zup_toolchain::file_name(&component, std::env::consts::EXE_SUFFIX);
            let path = staged.join(&name);
            std::fs::write(&path, image(&component)).expect("write a component");
            let descriptor =
                zup_toolchain::ComponentDescriptor::of(&component, crate::ZUP_VERSION, &path)
                    .expect("describe");
            std::fs::write(
                staged.join(format!("{name}{}", zup_toolchain::DESCRIPTOR_SUFFIX)),
                descriptor.encode(),
            )
            .expect("write a descriptor");
        }

        let with_staged = report(executable.clone(), &state, None);
        assert!(with_staged.is_complete());
        for component in &with_staged.components {
            assert_eq!(
                component.source,
                Some(ToolchainSourceName::Cache),
                "{component:?}"
            );
        }

        let root = directory.path().join("elsewhere");
        std::fs::create_dir_all(&root).expect("a root");
        for component in host_components() {
            let name = zup_toolchain::file_name(&component, std::env::consts::EXE_SUFFIX);
            let path = root.join(&name);
            std::fs::write(&path, image(&component)).expect("write a component");
            let descriptor =
                zup_toolchain::ComponentDescriptor::of(&component, crate::ZUP_VERSION, &path)
                    .expect("describe");
            std::fs::write(
                root.join(format!("{name}{}", zup_toolchain::DESCRIPTOR_SUFFIX)),
                descriptor.encode(),
            )
            .expect("write a descriptor");
        }
        let overridden = report(executable, &state, Some(root));
        assert!(overridden.is_complete(), "{}", overridden.human());
        for component in &overridden.components {
            assert_eq!(
                component.source,
                Some(ToolchainSourceName::Root),
                "{component:?}"
            );
        }
    }

    #[test]
    fn a_release_that_does_not_describe_its_own_bytes_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");

        let error = install(directory.path(), &state, None).expect_err("an empty directory");
        assert!(
            matches!(error, ToolchainCommandError::NotARelease { .. }),
            "{error}"
        );

        let material = release(&directory.path().join("material"));
        let index = ToolchainRelease::read(&material).expect("read");
        let component = index
            .components
            .iter()
            .find(|file| !file.path.ends_with(zup_toolchain::DESCRIPTOR_SUFFIX))
            .expect("a component");
        std::fs::write(
            zup_toolchain::resolve(&material, &component.path),
            b"other bytes",
        )
        .expect("corrupt");
        let error = install(&material, &state, None).expect_err("a changed component");
        assert!(
            matches!(error, ToolchainCommandError::Damaged { .. }),
            "{error}"
        );
        assert!(
            !state.join(crate::toolchain::CACHE_DIRECTORY).exists(),
            "a refused install writes nothing"
        );
    }

    #[test]
    fn a_descriptor_that_claims_another_release_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let material = release(&directory.path().join("material"));
        let state = directory.path().join("state");
        let staged = material.join(format!("toolchain/{}", crate::ZUP_VERSION));
        let mut index = ToolchainRelease::read(&material).expect("read");
        for file in &mut index.components {
            if !file.path.ends_with(zup_toolchain::DESCRIPTOR_SUFFIX) {
                continue;
            }
            let name = file_name_of(&file.path);
            let path = staged.join(&name);
            let mut descriptor =
                zup_toolchain::ComponentDescriptor::parse(&std::fs::read(&path).expect("read"))
                    .expect("parse");
            descriptor.zup_version = "9.9.9".to_owned();
            std::fs::write(&path, descriptor.encode()).expect("restamp");
            let relative = format!("toolchain/{}/{name}", crate::ZUP_VERSION);
            *file = zup_toolchain::ReleaseFile::of(&relative, &path).expect("re-measure");
        }
        std::fs::write(
            material.join(zup_toolchain::RELEASE_INDEX_NAME),
            index.encode(),
        )
        .expect("rewrite the index");

        let error = index
            .verify(&material)
            .expect_err("a descriptor that claims another release");
        assert!(error.to_string().contains("is from zup 9.9.9"), "{error}");
        let error = install(&material, &state, None).expect_err("a foreign descriptor");
        assert!(error.to_string().contains("is from zup 9.9.9"), "{error}");
    }

    #[test]
    fn a_release_from_another_zup_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let material = release(&directory.path().join("material"));
        let state = directory.path().join("state");
        let index = ToolchainRelease::read(&material).expect("read");
        let mut foreign = index.clone();
        foreign.zup_version = "9.9.9".to_owned();
        std::fs::write(
            material.join(zup_toolchain::RELEASE_INDEX_NAME),
            foreign.encode(),
        )
        .expect("write the foreign index");

        let error = install(&material, &state, None).expect_err("a foreign release");
        assert!(
            matches!(error, ToolchainCommandError::WrongVersion { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("9.9.9"), "{error}");
    }

    #[test]
    fn clean_names_the_other_cached_versions_and_removes_exactly_them() {
        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");
        let cache_root = state.join(crate::toolchain::CACHE_DIRECTORY);
        for version in ["0.0.1", "0.0.2", "9.9.9"] {
            std::fs::create_dir_all(cache_root.join(version)).expect("a version directory");
            std::fs::write(cache_root.join(version).join("bytes"), b"x").expect("bytes");
        }
        std::fs::write(cache_root.join(".zup-installing"), b"torn").expect("a temp file");

        let report = isolated_status(&state);
        assert_eq!(report.other_versions, vec!["0.0.2", "9.9.9"]);
        assert!(report.cache_populated);

        // Dry run first: it must change nothing.
        let dry = clean(&state, false, true).expect("a dry run");
        assert_eq!(
            dry.removed,
            vec!["0.0.2 (1 B)", "9.9.9 (1 B)", ".zup-installing"]
        );
        assert_eq!(dry.kept, vec![crate::ZUP_VERSION.to_owned()]);
        assert!(
            cache_root.join("0.0.2").is_dir(),
            "a dry run removes nothing"
        );
        assert!(cache_root.join(".zup-installing").is_file());

        let wet = clean(&state, false, false).expect("clean");
        assert_eq!(wet.removed, vec!["0.0.2", "9.9.9", ".zup-installing"]);
        assert!(
            cache_root.join("0.0.1").is_dir(),
            "the current version stays"
        );
        assert!(!cache_root.join("0.0.2").exists());
        assert!(!cache_root.join(".zup-installing").exists());

        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");
        install(&release(&directory.path().join("material")), &state, None).expect("install");
        let cache_root = state.join(crate::toolchain::CACHE_DIRECTORY);
        assert!(cache_root.join(crate::ZUP_VERSION).is_dir());

        let report = clean(&state, true, false).expect("clean --all");
        assert!(report.kept.is_empty());
        assert!(report.removed.contains(&crate::ZUP_VERSION.to_owned()));
        assert!(!cache_root.join(crate::ZUP_VERSION).exists());
        assert!(
            clean(&state, true, false)
                .expect("clean again")
                .removed
                .is_empty()
        );
    }

    #[test]
    fn installing_twice_is_the_same_install() {
        let directory = tempfile::tempdir().expect("temp dir");
        let material = release(&directory.path().join("material"));
        let state = directory.path().join("state");
        let first = install(&material, &state, None).expect("install");
        let second = install(&material, &state, None).expect("install again");
        assert_eq!(first, second);

        let cache = crate::toolchain::cache_directory(&state, crate::ZUP_VERSION);
        let entries: Vec<String> = std::fs::read_dir(&cache)
            .expect("the cache")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.iter().all(|name| !name.ends_with("zup-installing")),
            "a torn write was left behind: {entries:?}"
        );
    }
}
