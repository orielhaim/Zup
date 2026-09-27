//! Which repository a release belongs to, and how that is decided.
//!
//! # Precedence
//!
//! Three sources, in one fixed order, first answer wins:
//!
//! 1. **Explicit.** `--repo owner/name`, or `[publish.github] repository`.
//! 2. **The environment.** `GITHUB_REPOSITORY`, which Actions sets to
//!    `owner/name` for the repository the workflow is running in. That is the
//!    authoritative answer in CI and it is right there, so it is preferred over
//!    anything on disk.
//! 3. **The Git remote.** Read out of `.git/config`.
//!
//! # Why `.git/config` and not a Git library
//!
//! Because there is nothing to ask. A remote URL is one line of INI in a file
//! that is on disk, in a documented format, and `libgit2` would add a native
//! dependency, a vendored OpenSSL decision, and forty megabytes to read a value
//! a regular expression can read. If zup ever needs history, blame, or a
//! signature over a commit, that is when a Git library is worth its cost — and it
//! is not this code's problem today.
//!
//! # Why it refuses rather than guesses
//!
//! A repository with an `origin` on github.com and a fork on another host has
//! two plausible answers, and publishing a release to the wrong one is not a
//! mistake that announces itself: the release succeeds, the tag is created, and
//! the wrong project now has a `v1.4.0`. So: exactly one candidate, or a name
//! the developer chose.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::endpoint::{GITHUB_COM, GithubHost};
use crate::error::GithubError;

/// A repository on one GitHub installation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubRepository {
    /// The installation's web hostname.
    pub host: GithubHost,
    pub owner: String,
    pub name: String,
}

impl GithubRepository {
    /// A repository on `host`.
    pub fn new(host: GithubHost, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host,
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// A repository on github.com.
    pub fn dotcom(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(GithubHost::dotcom(), owner, name)
    }

    /// Parse `owner/name`, on `host`.
    pub fn parse(host: &GithubHost, value: &str) -> Result<Self, GithubError> {
        let value = value.trim().trim_end_matches(".git").trim_matches('/');
        if value.is_empty() {
            return Err(GithubError::Repository {
                reason: "a repository is `owner/name`".to_owned(),
            });
        }
        if value.contains("://") || value.contains('@') {
            return Err(GithubError::Repository {
                reason: format!(
                    "`{value}` looks like a URL; a repository is `owner/name` with no host"
                ),
            });
        }
        let (owner, name) = value
            .split_once('/')
            .ok_or_else(|| GithubError::Repository {
                reason: format!("`{value}` is missing the `/` between owner and name"),
            })?;
        check_segment(owner, "owner")?;
        check_segment(name, "name")?;
        Ok(Self::new(host.clone(), owner, name))
    }

    /// The `owner/name` path segment every release API addresses.
    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Whether this is github.com rather than an Enterprise installation.
    ///
    /// A repository on github.com is the case where a runtime client can fetch
    /// release assets with no credential at all, which is a difference in what a
    /// project can promise rather than a detail.
    pub fn is_public_host(&self) -> bool {
        self.host.dotcom
    }
}

impl fmt::Display for GithubRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// A GitHub repository named on the command line, possibly with a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySpec {
    /// The installation, when the value named one.
    pub host: Option<GithubHost>,
    pub owner: String,
    pub name: String,
}

impl RepositorySpec {
    /// Parse `owner/name` or `host/owner/name`.
    ///
    /// Three segments is how a project writes an Enterprise repository, and
    /// accepting it here is what lets a single `--repo` flag cover both
    /// installations. Two segments means the caller's host, which is github.com
    /// unless discovery already found otherwise.
    pub fn parse(value: &str) -> Result<Self, GithubError> {
        let value = value.trim().trim_end_matches(".git").trim_matches('/');
        let parts: Vec<&str> = value.split('/').collect();
        match parts.as_slice() {
            [owner, name] => {
                check_segment(owner, "owner")?;
                check_segment(name, "name")?;
                Ok(Self {
                    host: None,
                    owner: (*owner).to_owned(),
                    name: (*name).to_owned(),
                })
            }
            [host, owner, name] => {
                let host = GithubHost::enterprise(host)?;
                check_segment(owner, "owner")?;
                check_segment(name, "name")?;
                Ok(Self {
                    host: Some(host),
                    owner: (*owner).to_owned(),
                    name: (*name).to_owned(),
                })
            }
            _ => Err(GithubError::Repository {
                reason: format!("`{value}` is not `owner/name` or `host/owner/name`"),
            }),
        }
    }

    /// Resolve this spec against `default`.
    pub fn resolve(&self, default: &GithubHost) -> GithubRepository {
        GithubRepository::new(
            self.host.clone().unwrap_or_else(|| default.clone()),
            self.owner.clone(),
            self.name.clone(),
        )
    }
}

/// Where a resolved repository came from, for a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    /// Named on the command line or in `zup.toml`.
    Configured,
    /// `GITHUB_REPOSITORY`.
    Environment,
    /// A Git remote.
    Remote,
    /// Supplied by a caller that already knew.
    Given,
}

impl Discovery {
    /// The name a report uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Environment => "github_repository",
            Self::Remote => "git remote",
            Self::Given => "given",
        }
    }
}

/// A repository, and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub repository: GithubRepository,
    pub discovery: Discovery,
    /// The remote that supplied it, when a remote did.
    pub remote: Option<String>,
}

/// Decide which repository a release belongs to.
pub fn resolve(
    explicit: Option<&RepositorySpec>,
    environ: &dyn Environment,
    working_directory: &Path,
) -> Result<Resolved, GithubError> {
    if let Some(spec) = explicit {
        let host = spec
            .host
            .clone()
            .unwrap_or_else(|| install_from_environ(environ).unwrap_or_else(GithubHost::dotcom));
        return Ok(Resolved {
            repository: spec.resolve(&host),
            discovery: Discovery::Configured,
            remote: None,
        });
    }
    if let Some(value) = environ.get("GITHUB_REPOSITORY")
        && !value.trim().is_empty()
    {
        let spec =
            RepositorySpec::parse(value.trim()).map_err(|error| GithubError::Repository {
                reason: format!("GITHUB_REPOSITORY is not usable: {error}"),
            })?;
        let host = spec
            .host
            .clone()
            .unwrap_or_else(|| install_from_environ(environ).unwrap_or_else(GithubHost::dotcom));
        return Ok(Resolved {
            repository: spec.resolve(&host),
            discovery: Discovery::Environment,
            remote: None,
        });
    }
    resolve_from_remotes(environ, working_directory)
}

/// The installation the environment says this is, if it says one.
///
/// `GITHUB_API_URL` is the one that is actually authoritative: Actions sets it
/// from the server the workflow runs against, so on an Enterprise installation
/// it is already the API base with `/api/v3/` on the end of it.
pub fn install_from_environ(environ: &dyn Environment) -> Option<GithubHost> {
    if let Some(base) = environ.get("GITHUB_API_URL")
        && let Ok(url) = url::Url::parse(base.trim())
        && let Some(host) = url.host_str()
    {
        return GithubHost::enterprise(host).ok();
    }
    environ
        .get("GITHUB_SERVER_URL")
        .and_then(|base| {
            url::Url::parse(base.trim())
                .ok()?
                .host_str()
                .map(str::to_owned)
        })
        .and_then(|host| GithubHost::enterprise(&host).ok())
}

/// Choose a repository from the Git remotes in `working_directory`.
pub fn resolve_from_remotes(
    _environ: &dyn Environment,
    working_directory: &Path,
) -> Result<Resolved, GithubError> {
    let Some(config) = read_git_config(working_directory) else {
        return Err(GithubError::NoRepository);
    };
    let remotes = parse_remotes(&config);
    if remotes.is_empty() {
        return Err(GithubError::NoRepository);
    }
    let mut candidates: Vec<(String, String)> = Vec::new();
    for (remote, url) in &remotes {
        if let Some((host, owner, name)) = parse_remote_url(url) {
            candidates.push((remote.clone(), format!("{host}/{owner}/{name}")));
        }
    }
    if candidates.is_empty() {
        return Err(GithubError::NoRepository);
    }
    // `origin` is the name a clone gives its upstream, so one named `origin` on a
    // GitHub host is the answer by convention rather than by guess.
    if let Some((remote, path)) = candidates.iter().find(|(name, _)| name == "origin") {
        return build(remote.clone(), path, Discovery::Remote);
    }
    if candidates.len() == 1 {
        let (remote, path) = candidates[0].clone();
        return build(remote, &path, Discovery::Remote);
    }
    let mut names: Vec<String> = candidates
        .iter()
        .map(|(remote, path)| format!("{remote} → {path}"))
        .collect();
    names.sort();
    Err(GithubError::AmbiguousRepository { candidates: names })
}

fn build(remote: String, path: &str, discovery: Discovery) -> Result<Resolved, GithubError> {
    let spec = RepositorySpec::parse(path)?;
    let host = spec.host.clone().unwrap_or_else(GithubHost::dotcom);
    Ok(Resolved {
        repository: spec.resolve(&host),
        discovery,
        remote: Some(remote),
    })
}

/// The GitHub parts of a Git remote URL.
///
/// The three shapes Git writes are an HTTPS URL, an SCP-style `user@host:path`,
/// and an `ssh://` URL. All three reduce to the same three strings, and anything
/// that is not a GitHub-shaped host is not a candidate at all.
pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let (host, path) = if let Some(rest) = url.strip_prefix("ssh://") {
        // `ssh://git@github.com/owner/repo.git`
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.to_owned(), path.to_owned())
    } else if url.contains("://") {
        let parsed = url::Url::parse(url).ok()?;
        (
            parsed.host_str()?.to_owned(),
            parsed.path().trim_start_matches('/').to_owned(),
        )
    } else if let Some((authority, path)) = url.split_once(':') {
        // `git@github.com:owner/repo.git`
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host.to_owned(), path.to_owned())
    } else {
        return None;
    };
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    if !is_github_host(&host) {
        return None;
    }
    Some((host.to_ascii_lowercase(), owner.to_owned(), name.to_owned()))
}

/// Whether a hostname is one zup will publish to.
///
/// `github.com` and anything that names itself as a GitHub installation. A
/// hostname is *not* assumed from a remote: an unknown host is a question, not a
/// match, and a project on a self-hosted forge must say so with an explicit
/// `--repo` rather than discover it by accident.
fn is_github_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    host == GITHUB_COM || host.ends_with(".github.com") || host.starts_with("github.")
}

/// The environment, narrowed to what discovery reads.
///
/// A trait rather than the process environment so every branch of the precedence
/// order is reachable from a test, and so a library caller that has its own
/// settings is not forced to mutate global state to use it.
pub trait Environment {
    fn get(&self, name: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// The `.git/config` for `directory`, following a worktree pointer.
pub fn read_git_config(directory: &Path) -> Option<String> {
    let git = directory.join(".git");
    if git.is_dir() {
        return std::fs::read_to_string(git.join("config")).ok();
    }
    // A linked worktree's `.git` is a file naming the real directory.
    let pointer = std::fs::read_to_string(&git).ok()?;
    let target = pointer
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        directory.join(target)
    };
    std::fs::read_to_string(target.join("config")).ok()
}

/// Where a Git config lives for a directory, walking up to the repository root.
pub fn find_git_config(directory: &Path) -> Option<PathBuf> {
    let mut current = Some(directory);
    while let Some(directory) = current {
        let git = directory.join(".git");
        if git.exists() {
            return Some(git);
        }
        current = directory.parent();
    }
    None
}

/// Every remote in a Git config, as `(name, url)`, in file order.
pub fn parse_remotes(config: &str) -> Vec<(String, String)> {
    let mut remotes = Vec::new();
    let mut section: Option<(String, String)> = None;
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = parse_section(inner);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').to_owned();
        if key == "url"
            && !value.is_empty()
            && let Some((kind, name)) = &section
            && kind == "remote"
        {
            remotes.push((name.clone(), value));
        }
    }
    remotes
}

/// `[remote "origin"]` → `("remote", "origin")`.
fn parse_section(inner: &str) -> Option<(String, String)> {
    let inner = inner.trim();
    let (kind, rest) = inner.split_once(char::is_whitespace)?;
    let name = rest.trim().trim_matches('"');
    if name.is_empty() {
        return None;
    }
    Some((kind.to_ascii_lowercase(), name.to_owned()))
}

fn check_segment(value: &str, what: &str) -> Result<(), GithubError> {
    if value.is_empty() {
        return Err(GithubError::Repository {
            reason: format!("a repository {what} cannot be empty"),
        });
    }
    if value.len() > 39 {
        return Err(GithubError::Repository {
            reason: format!("a repository {what} may be at most 39 characters"),
        });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(GithubError::Repository {
            reason: format!(
                "a repository {what} may use only letters, digits, `.`, `-`, and `_`; \
                 `{value}` does not"
            ),
        });
    }
    Ok(())
}
