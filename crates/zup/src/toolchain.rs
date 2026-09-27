//! Where the zup-owned binaries a build composes an artifact from come from.
//!
//! `zup build` needs a native runtime template per target and a dispatcher per
//! launcher experience. Those are zup's own binaries, not the project's, and
//! making every project author compile them before their first build is a tax on
//! the ordinary path to an installer.
//!
//! So the build asks for a *semantic component* and the resolver finds the bytes.
//! Resolution is a fixed precedence, first match wins:
//!
//! 1. an explicit `--runtime` / `--dispatcher` path — a developer's escape hatch
//! 2. an explicit toolchain root — `--toolchain`, or `ZUP_TOOLCHAIN`
//! 3. the installed toolchain cache for this exact zup version
//! 4. a toolchain staged beside this executable, versioned then unversioned
//!
//! There is no network step, deliberately. A build that silently depends on GitHub
//! being reachable is a build a release engineer discovers is broken during an
//! outage, and a toolchain that has to be fetched is a toolchain whose provenance
//! nobody can state. A pinned version plus a populated cache is a reproducible
//! offline build; an empty cache is a clear refusal naming both remedies.
//!
//! Whatever the source, a resolved component is checked before it is used: the
//! descriptor names the zup release, the target, and the frontend, and the
//! component's own bytes must hash to the digest the descriptor recorded. The
//! target and the launcher experience are then confirmed against the file's own
//! header, which a build host reads without executing the file.

use std::path::{Path, PathBuf};

use zup_core::Frontend;
use zup_toolchain::{ComponentDescriptor, Subsystem, ToolchainComponent, ToolchainError};

/// The environment variable that names a toolchain root.
pub const TOOLCHAIN_ENV: &str = "ZUP_TOOLCHAIN";

/// The suffix this machine writes executables with, which is part of the name a
/// component is stored under.
const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

/// The directory a staged toolchain lives in, beside the executable.
pub const STAGED_DIRECTORY: &str = "toolchain";

/// The cache directory inside a zup state root.
pub const CACHE_DIRECTORY: &str = "toolchain";

/// Where a resolved component came from.
///
/// The order of the variants is the resolution order, and `doctor` reports it, so
/// "where did this build get its runtime" is a question with a printed answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolchainSource {
    /// Named explicitly on the command line.
    Override,
    /// Inside the toolchain root the caller named.
    Root,
    /// The installed toolchain cache for this zup version.
    Cache,
    /// Staged beside this executable.
    Staged,
}

impl ToolchainSource {
    /// The one-word name a readiness report shows.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Root => "toolchain root",
            Self::Cache => "cache",
            Self::Staged => "staged",
        }
    }
}

/// One resolved component.
#[derive(Debug, Clone)]
pub struct ResolvedComponent {
    pub path: PathBuf,
    pub source: ToolchainSource,
    pub descriptor: ComponentDescriptor,
}

/// Why a component could not be resolved.
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

/// A component resolver, bound to one zup version and one search order.
pub struct ToolchainResolver {
    version: String,
    override_root: Option<PathBuf>,
    executable: PathBuf,
    state_root: PathBuf,
}

impl ToolchainResolver {
    /// A resolver for this build of zup.
    pub fn new(version: String, executable: PathBuf, state_root: PathBuf) -> Self {
        Self {
            version,
            override_root: std::env::var_os(TOOLCHAIN_ENV).map(PathBuf::from),
            executable,
            state_root,
        }
    }

    /// Resolve against an explicit toolchain root instead of the default order.
    pub fn with_root(mut self, root: Option<PathBuf>) -> Self {
        if root.is_some() {
            self.override_root = root;
        }
        self
    }

    /// The zup release whose components this resolver accepts.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Resolve one component.
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
        for (root, source) in self.candidates() {
            let path = root.join(zup_toolchain::file_name(component, EXECUTABLE_SUFFIX));
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
            },
            wanted: describe(component),
            searched: searched.join("\n  "),
        })
    }

    /// The roots to search, in precedence order.
    fn candidates(&self) -> Vec<(PathBuf, ToolchainSource)> {
        let mut candidates = Vec::new();
        if let Some(root) = &self.override_root {
            candidates.push((root.clone(), ToolchainSource::Root));
        }
        let cache = self.state_root.join(CACHE_DIRECTORY).join(&self.version);
        candidates.push((cache, ToolchainSource::Cache));

        // Beside the executable. A `deps` parent is stripped because a test or a
        // `cargo run` binary lives one level deeper than the staged toolchain.
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

/// Read, check, and independently confirm one component.
fn check(
    path: &Path,
    component: &ToolchainComponent,
    version: &str,
) -> Result<ComponentDescriptor, ToolchainError> {
    let descriptor = zup_toolchain::read(path, component, version)?;
    // The descriptor is a claim written by the toolchain build. The file's own
    // header is an independent statement, and the two agreeing is what makes the
    // claim worth anything on a host that cannot run the file.
    match component {
        ToolchainComponent::Runtime { target, frontend } => {
            let found =
                zup_windows::read_pe_target(path).map_err(|error| ToolchainError::Unreadable {
                    path: path.to_path_buf(),
                    reason: error.to_string(),
                })?;
            if &found != target {
                return Err(ToolchainError::WrongTarget {
                    found: found.as_str().to_owned(),
                    wanted: target.as_str().to_owned(),
                });
            }
            zup_windows::validate_pe_frontend(path, *frontend).map_err(|error| {
                ToolchainError::Unreadable {
                    path: path.to_path_buf(),
                    reason: error.to_string(),
                }
            })?;
        }
        ToolchainComponent::Dispatcher { subsystem, .. } => {
            let found = zup_windows::read_pe_subsystem(path).map_err(|error| {
                ToolchainError::Unreadable {
                    path: path.to_path_buf(),
                    reason: error.to_string(),
                }
            })?;
            let matches = matches!(
                (subsystem, found),
                (Subsystem::Gui, zup_windows::PeSubsystem::Gui)
                    | (Subsystem::Console, zup_windows::PeSubsystem::Console)
            );
            if !matches {
                return Err(ToolchainError::WrongSubsystem {
                    found: format!("{found:?}").to_lowercase(),
                    wanted: subsystem.as_str().to_owned(),
                });
            }
        }
    }
    Ok(descriptor)
}

/// The component one target profile needs.
pub fn runtime_for(target: &zup_core::TargetTriple, frontend: Frontend) -> ToolchainComponent {
    ToolchainComponent::Runtime {
        target: target.clone(),
        frontend,
    }
}

/// The component one artifact's launcher experience needs.
///
/// `online` is asked for only by a thin artifact. A thin artifact's launcher
/// resolves a release over the network before it can start a runtime, so
/// composing one from the offline launcher would produce an installer that
/// refuses to install itself on a user's machine — a failure with no build-time
/// symptom, which is exactly the class this whole contract exists to catch.
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

/// A component, as one line, for a readiness report or an error.
pub fn describe(component: &ToolchainComponent) -> String {
    match component {
        ToolchainComponent::Runtime { target, frontend } => {
            format!("{} runtime for {}", frontend.as_str(), target.as_str())
        }
        ToolchainComponent::Dispatcher { subsystem, .. } => {
            format!("{} dispatcher", subsystem.as_str())
        }
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

/// The refusal a user sees when a build cannot find a component.
///
/// It names the two things that fix it, in the order a person should try them,
/// and it does not pretend a network fetch is one of them.
pub fn missing_component_message(component: &ToolchainComponent, error: &ResolveError) -> String {
    let kind = match component {
        ToolchainComponent::Runtime { .. } => "runtime template",
        ToolchainComponent::Dispatcher { .. } => "dispatcher",
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
        let sources: Vec<ToolchainSource> = resolver
            .candidates()
            .iter()
            .map(|(_, source)| *source)
            .collect();
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
        assert_eq!(resolver.candidates()[0].0, root);
        assert_eq!(
            resolver.candidates()[1].0,
            state.join("toolchain").join("0.1.0"),
            "the cache is keyed by the exact zup version, so a pinned build is reproducible"
        );
    }

    #[test]
    fn an_environment_root_is_a_root_too() {
        // The constructor reads ZUP_TOOLCHAIN; with it unset the resolver falls
        // through to the cache, which is what a normal user has.
        let resolver = ToolchainResolver::new(
            "0.1.0".into(),
            PathBuf::from("C:/bin/zup.exe"),
            PathBuf::from("C:/state"),
        );
        assert!(
            resolver
                .candidates()
                .iter()
                .all(|(_, source)| *source != ToolchainSource::Root),
            "without ZUP_TOOLCHAIN there is no explicit root"
        );
    }

    #[test]
    fn a_deps_binary_resolves_against_the_directory_above_it() {
        let resolver = ToolchainResolver::new(
            "0.1.0".into(),
            PathBuf::from("C:/target/debug/deps/zup.exe"),
            PathBuf::from("C:/state"),
        );
        let staged = resolver.candidates();
        let last = staged.last().expect("a staged candidate").0.clone();
        assert_eq!(last, PathBuf::from("C:/target/debug/toolchain"));
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
}
