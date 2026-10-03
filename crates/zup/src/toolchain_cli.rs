//! `zup toolchain`: install, inspect and remove the binaries a build composes from.
//!
//! The resolver searches four places in a fixed order (see [`crate::toolchain`]).
//! Three of them are somebody's decision: a flag, an environment variable, a
//! staged directory beside the executable. The fourth is the **cache** - the
//! installed toolchain for this exact zup version - and until this command
//! existed, nothing in the product produced it. A resolver arm with no producer
//! is a documented feature that never works.
//!
//! So this is the producer. `install` takes a release directory - the one thing
//! `cargo xtask toolchain package` writes and `cargo xtask release clean-room`
//! verifies - and copies its components into the cache. It verifies the release
//! index first, and verifies the copy afterwards, because a cache populated from
//! bytes nobody checked is a cache that hands a build a component it should
//! refuse.
//!
//! Two things it deliberately does not do:
//!
//! - **It does not fetch.** Resolution is offline by design; a build that
//!   silently depends on GitHub being reachable is a build a release engineer
//!   discovers is broken during an outage. `install` takes a directory a person
//!   already has.
//! - **It does not install a different version.** The cache is keyed by this
//!   zup's version, and a component from another release is refused by the
//!   descriptor check. Installing one would be writing bytes into a directory
//!   whose only reader will reject them.
//!
//! `clean` exists because the cache is keyed by version and versions accumulate.
//! A toolchain is tens of megabytes per release, and the default is to remove
//! every version except the one this executable can use.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{Args, ValueHint};
use zup_automation::{
    AutomationResult, Details, Identifier, LogLevel, ToolchainCleanDetails,
    ToolchainInstallDetails, ToolchainStatusDetails,
};
use zup_toolchain::{ToolchainComponent, ToolchainRelease};

use crate::cli::OutputArg;
use crate::report::Reporter;
use crate::toolchain::{ToolchainResolver, ToolchainSource, describe};

/// The operations `zup toolchain` performs.
///
/// Each verb is its own zup operation in the machine contract rather than three shapes
/// of one, because a setup tool wants to branch on "which of these happened" and
/// `toolchain.status` is a different question from `toolchain.install`.
#[derive(Debug, Args)]
pub struct ToolchainCommand {
    #[command(subcommand)]
    pub command: ToolchainVerb,
}

/// What to do with the toolchain.
#[derive(Debug, clap::Subcommand)]
pub enum ToolchainVerb {
    /// Copy a zup release's components into this machine's cache.
    Install(ToolchainInstallCommand),
    /// Report what a build here would resolve, and from where.
    Status(ToolchainStatusCommand),
    /// Remove cached toolchains this zup cannot use.
    Clean(ToolchainCleanCommand),
}

/// Install a release into the toolchain cache.
///
/// Takes a directory, not a URL. Resolution is offline by design - a build that
/// silently depends on a remote host being reachable is a build a release
/// engineer discovers is broken during an outage - so the producer of a cache
/// entry is a person who already has the bytes.
#[derive(Debug, Args)]
pub struct ToolchainInstallCommand {
    /// The release directory: the one holding `zup-toolchain.json`.
    #[arg(value_name = "RELEASE", value_hint = ValueHint::DirPath)]
    pub source: PathBuf,
    /// The state root whose cache to populate.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// Readable text, or the versioned machine result.
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

/// Report the toolchain a build on this machine would use.
#[derive(Debug, Args)]
pub struct ToolchainStatusCommand {
    /// The state root to read the cache from.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// Readable text, or the versioned machine result.
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
}

/// Remove cached toolchains.
#[derive(Debug, Args)]
pub struct ToolchainCleanCommand {
    /// Readable text, or the versioned machine result.
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputArg,
    /// The state root whose cache to clean.
    #[arg(long, value_hint = ValueHint::DirPath)]
    pub state_root: Option<PathBuf>,
    /// Also remove this zup's own cache.
    ///
    /// Off by default: a machine can have two zup releases on it, and a clean
    /// run by one deleting the other's components would break it.
    #[arg(long)]
    pub all: bool,
    /// Report what would be removed without removing it.
    #[arg(long)]
    pub dry_run: bool,
}

/// One component's state, as `status` reports it.
#[derive(Debug, Clone)]
pub struct ComponentStatus {
    /// The component, in the wording every refusal uses.
    pub component: String,
    /// Whether a usable copy was found.
    pub found: bool,
    /// Which arm of the search produced it.
    pub source: Option<ToolchainSourceName>,
    /// The file, when one was found.
    pub path: Option<String>,
    /// Why it was refused, when it was.
    pub problem: Option<String>,
}

impl ToolchainSourceName {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Root => "root",
            Self::Cache => "cache",
            Self::Staged => "staged",
        }
    }
}

/// The name of a resolver arm, for a report.
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

/// The whole cache, for one zup version.
#[derive(Debug, Clone)]
pub struct ToolchainStatus {
    /// The zup release these components have to come from.
    pub zup_version: String,
    /// The build host's canonical target triple.
    pub host: String,
    /// Where the cache for this version lives.
    pub cache: String,
    /// Whether the cache directory exists at all.
    pub cache_populated: bool,
    /// Every other zup version with a cache, which `clean` would remove.
    pub other_versions: Vec<String>,
    pub components: Vec<ComponentStatus>,
}

impl ToolchainStatus {
    /// Whether every component a build host needs is usable.
    pub fn is_complete(&self) -> bool {
        self.components.iter().all(|component| component.found)
    }

    /// The readable view.
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

/// Why a toolchain operation failed.
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

/// Run `zup toolchain`.
///
/// Dispatched by the CLI's shared operation wrapper rather than here, because each
/// verb is a different zup operation in the machine contract and each one therefore
/// opens its own stream and reports its own result.
impl ToolchainCommand {
    /// The verb, for a caller that wants to branch on it without matching twice.
    pub const fn operation(&self) -> &'static str {
        match self.command {
            ToolchainVerb::Install(_) => zup_automation::OPERATION_TOOLCHAIN_INSTALL,
            ToolchainVerb::Status(_) => zup_automation::OPERATION_TOOLCHAIN_STATUS,
            ToolchainVerb::Clean(_) => zup_automation::OPERATION_TOOLCHAIN_CLEAN,
        }
    }
}

/// The state root a toolchain command reads, for every verb.
///
/// The flag and the default answer one question, so they are answered in one
/// place: three verbs each deciding it separately is three places a fix has to
/// land.
fn state_root(named: Option<PathBuf>) -> miette::Result<PathBuf> {
    match named {
        Some(root) => Ok(root),
        None => crate::toolchain_state_root(),
    }
}

/// Copy a release's components into this machine's cache.
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

/// A toolchain failure, with the code its variant means.
///
/// The variants are already the taxonomy; a code per variant is what lets a setup tool
/// react to "you have the wrong zup" differently from "the archive is damaged" without
/// reading the message.
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

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub zup_version: String,
    /// The release the bytes came from.
    pub source: String,
    /// The directory they now live in.
    pub cache: String,
    /// One line per component installed.
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

/// Verify a release directory and copy its components into the cache.
///
/// The order is: read the index, refuse a foreign version, verify every named
/// file, copy, then re-verify the copy. The last step is the one that matters
/// most - a copy that lands truncated, or a partial copy left by a crash, is
/// exactly the failure a build should refuse rather than compose an installer
/// from.
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

    // Every file the index names, descriptors included, because the resolver
    // reads a component's descriptor before it reads the component. A cache of
    // seven executables and no descriptors is a cache nothing can read.
    //
    // What is *not* copied is the CLI. The cache is what a *build* reads; the
    // executable a person runs is a separate question, and a cache that also
    // held a `zup.exe` would be a second copy of the tool whose version decides
    // whether any of it is usable.
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

    // And prove the cache is what the release said it was, by resolving out of
    // it the way a build will. This catches a truncated copy, a partial cache
    // from an interrupted run, and a component the index listed but the
    // directory does not hold.
    let verifier = ToolchainResolver::new(
        crate::ZUP_VERSION.to_owned(),
        // An executable path that resolves nothing: the point is to read the
        // cache, and a staged directory beside a real executable could answer
        // instead.
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
            // Every refusal, not the first: a copy that landed wrong usually
            // landed wrong for one reason, and a reader who has to re-run to see
            // the other six is a reader who re-runs.
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

/// Copy one component, replacing any file already there.
///
/// Not `copy_new_durable`: this is not a transaction payload and there is no
/// journal to recover it from. What matters is that the file on disk is always
/// either the previous component or the new one and never a half-written
/// mixture, and a temp-then-rename is what gives that. A cache entry left
/// truncated by a power cut is caught by the verification pass above on the next
/// run, and by the descriptor check on every build that touches it before that.
fn copy_component(from: &Path, to: &Path) -> Result<(), ToolchainCommandError> {
    let temporary = to.with_extension("zup-installing");
    let _ = std::fs::remove_file(&temporary);
    let result = (|| {
        std::fs::copy(from, &temporary).map_err(|source| ToolchainCommandError::Io {
            path: from.to_path_buf(),
            source,
        })?;
        // Flush before publishing, so a rename cannot make a component visible
        // before its bytes are on the medium. The directory entry is not flushed
        // - Windows has no portable way to do that - and the descriptor beside it
        // would catch a component that lost its tail.
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

/// Report what a build on this machine would resolve, and from where.
///
/// A status report that describes an unusable toolchain is a *failure result*, not an
/// error with no document: the report is the output, the status is the verdict, and a
/// setup tool needs both.
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

/// Build the report.
pub fn status(state_root: &Path, toolchain_root: Option<PathBuf>) -> ToolchainStatus {
    report(
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zup")),
        state_root,
        toolchain_root,
    )
}

/// The report for one resolver, however that resolver was built.
///
/// Split from [`status`] because the executable path is what selects the staged
/// arm, and a test that could not choose it would be reading whatever toolchain
/// the machine running the test happens to have staged beside its own binary.
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

/// The components a build on this machine can need.
fn host_components() -> Vec<ToolchainComponent> {
    let target = zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET)
        .expect("the build host's own target triple is valid");
    zup_toolchain::host_components(&target)
}

/// Remove cached toolchains this executable cannot use.
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

/// What a clean removed, or would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    /// The directory that was cleaned.
    pub cache: String,
    /// True when nothing was changed.
    pub dry_run: bool,
    /// One line per removed version or file.
    pub removed: Vec<String>,
    /// The versions that were left alone.
    pub kept: Vec<String>,
}

impl Cleaned {
    /// The readable view.
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

/// Remove every cached version this executable cannot use.
///
/// The default is deliberately not "remove everything": the cache is keyed by
/// zup version, so a machine with two zup releases installed has two
/// directories, and a clean run by the newer one that deleted the older one's
/// components would break the older one. `--all` says that is what you meant.
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

/// Files in the cache root that are not version directories.
///
/// A crash between creating a directory and filling it leaves one, and a release
/// that was interrupted the same way leaves a `.zup-installing` temp. Both are
/// unambiguous - nothing else in that directory is a file - and both are
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
mod tests {
    use super::*;

    /// A component's bytes: a PE image that agrees with its descriptor.
    ///
    /// The resolver reads a component two ways - the descriptor beside it and the
    /// file's own header - and refuses it if either disagrees. A fixture of plain
    /// bytes would pass the first and fail the second, so a test that installed
    /// one and asserted "a build will find it" would be asserting something the
    /// build refuses.
    fn image(component: &ToolchainComponent) -> Vec<u8> {
        if let ToolchainComponent::Preset = component {
            // A package, not a PE image, and a real one: the resolver verifies it
            // with the same reader a build consumes it with, so a fixture that
            // were anything else would be a fixture the resolver refuses.
            let mut writer = zup_artifact::ui::PresetPackageWriter::new(
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

    /// A release directory in the shape `cargo xtask toolchain package` writes.
    ///
    /// Built here rather than imported, so the test proves the command accepts
    /// the documented layout rather than that it accepts its own output.
    fn release(root: &Path) -> PathBuf {
        let staged = root.join(format!("toolchain/{}", crate::ZUP_VERSION));
        std::fs::create_dir_all(&staged).expect("the staged directory");
        let target = zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET)
            .expect("a valid target");
        let mut index = ToolchainRelease::new(crate::ZUP_VERSION, target.as_str());
        let cli = root.join(format!("zup{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&cli, b"a cli").expect("write the cli");
        index.cli = zup_toolchain::ReleaseFile::of("zup.exe", &cli).expect("measure the cli");
        for component in zup_toolchain::host_components(&target) {
            let name = zup_toolchain::file_name(&component, std::env::consts::EXE_SUFFIX);
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
        // The index sits at the release root, beside `zup.exe`, and the
        // components are under `toolchain/<version>/`.
        std::fs::write(root.join(zup_toolchain::RELEASE_INDEX_NAME), index.encode())
            .expect("write the index");
        root.to_path_buf()
    }

    /// The report as a caller with no toolchain beside its executable sees it.
    ///
    /// The executable path is the whole point of this helper. The test binary in
    /// `target/debug/deps` sits beside this repository's staged toolchain, so a
    /// report built from `current_exe` would find that and never exercise the
    /// cache at all.
    fn isolated_status(state: &Path) -> ToolchainStatus {
        report(state.join("no-such-toolchain").join("zup.exe"), state, None)
    }

    fn file_name_of(path: &str) -> String {
        path.rsplit('/').next().expect("a file name").to_owned()
    }

    /// The cache is the only arm that has a producer, and this is the proof that
    /// the producer and the consumer agree: a directory `xtask` packages resolves
    /// from `install`'s cache and from nowhere else.
    #[test]
    fn an_installed_toolchain_beats_a_staged_one_and_an_explicit_root() {
        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");
        install(&release(&directory.path().join("material")), &state, None).expect("install");

        // A staged toolchain beside the executable, which the resolver prefers
        // last. Resolving must still report the cache.
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

        // A named root that holds the components wins over the cache - that is
        // what the flag is for. A named root that holds *nothing* falls through,
        // because the search is a fixed precedence with first match, and a root
        // that cannot answer one component is not a reason to refuse the other
        // six.
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

    /// Every way a release can lie about itself is refused, and a refusal reaches
    /// the cache as no write at all. `install` verifies rather than copies because
    /// the copy is a place a bad file can hide from the person who ran the command.
    #[test]
    fn a_release_that_does_not_describe_its_own_bytes_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");

        // A directory that is not a release at all.
        let error = install(directory.path(), &state, None).expect_err("an empty directory");
        assert!(
            matches!(error, ToolchainCommandError::NotARelease { .. }),
            "{error}"
        );

        // A component whose bytes no longer hash to what the index recorded. The
        // index is re-measured, so the only thing left wrong is the component.
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

    /// A descriptor that lies about its own release is caught by the index, not
    /// by the copy. This is the mismatch the release index exists to find, and
    /// the setup matters: the index is re-measured after the restamp, so the only
    /// thing left wrong is that the descriptor and the release disagree about
    /// which zup produced them.
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

        // The index's own check is what finds it, and the install inherits the
        // refusal rather than re-deriving one.
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

    /// A cache keyed by version accumulates versions, and the only thing that
    /// makes that safe is that the resolver reads exactly one of them.
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

        // `--all` is the escape hatch, and it is an escape hatch: a machine can
        // have two zup releases on it, and one deleting the other's components
        // breaks it - so the current version is removed only when asked for.
        let directory = tempfile::tempdir().expect("temp dir");
        let state = directory.path().join("state");
        install(&release(&directory.path().join("material")), &state, None).expect("install");
        let cache_root = state.join(crate::toolchain::CACHE_DIRECTORY);
        assert!(cache_root.join(crate::ZUP_VERSION).is_dir());

        let report = clean(&state, true, false).expect("clean --all");
        assert!(report.kept.is_empty());
        assert!(report.removed.contains(&crate::ZUP_VERSION.to_owned()));
        assert!(!cache_root.join(crate::ZUP_VERSION).exists());
        // A clean with nothing left is not an error.
        assert!(
            clean(&state, true, false)
                .expect("clean again")
                .removed
                .is_empty()
        );
    }

    /// Installing twice is not an error, and the second install does not leave
    /// a temporary file behind. A command that only works on a clean machine is
    /// a command nobody runs twice.
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
