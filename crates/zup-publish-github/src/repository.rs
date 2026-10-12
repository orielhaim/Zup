use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use git_url_parse::GitUrl;

use crate::endpoint::{GITHUB_COM, GithubHost};
use crate::error::GithubError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubRepository {
    pub host: GithubHost,
    pub owner: String,
    pub name: String,
}

impl GithubRepository {
    pub fn new(host: GithubHost, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host,
            owner: owner.into(),
            name: name.into(),
        }
    }

    pub fn dotcom(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(GithubHost::dotcom(), owner, name)
    }

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

    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn is_public_host(&self) -> bool {
        self.host.dotcom
    }
}

impl fmt::Display for GithubRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySpec {
    pub host: Option<GithubHost>,
    pub owner: String,
    pub name: String,
}

impl RepositorySpec {
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

    pub fn resolve(&self, default: &GithubHost) -> GithubRepository {
        GithubRepository::new(
            self.host.clone().unwrap_or_else(|| default.clone()),
            self.owner.clone(),
            self.name.clone(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    Configured,
    Environment,
    Remote,
    Given,
}

impl Discovery {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Environment => "github_repository",
            Self::Remote => "git remote",
            Self::Given => "given",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub repository: GithubRepository,
    pub discovery: Discovery,
    pub remote: Option<String>,
}

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

pub fn resolve_from_remotes(
    _environ: &dyn Environment,
    working_directory: &Path,
) -> Result<Resolved, GithubError> {
    let mut remotes = git_remotes(working_directory);
    if remotes.is_empty() {
        remotes = config_remotes(working_directory);
    }
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

fn git_remotes(working_directory: &Path) -> Vec<(String, String)> {
    let Some(directory) = git_root(working_directory) else {
        return Vec::new();
    };
    let Ok(output) = Command::new("git")
        .args(["-C", &directory.to_string_lossy(), "remote", "-v"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(listed) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    let mut remotes: Vec<(String, String)> = Vec::new();
    for line in listed.lines() {
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let url = rest.split_whitespace().next().unwrap_or_default();
        if url.is_empty() || url == "." {
            continue;
        }
        if !remotes.iter().any(|(existing, _)| existing == name) {
            remotes.push((name.to_owned(), url.to_owned()));
        }
    }
    remotes
}

fn git_root(working_directory: &Path) -> Option<PathBuf> {
    Some(
        find_git_config(working_directory)
            .and_then(|marker| marker.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| working_directory.to_path_buf()),
    )
}

fn config_remotes(working_directory: &Path) -> Vec<(String, String)> {
    read_git_config(working_directory)
        .map(|config| parse_remotes(&config))
        .unwrap_or_default()
}

pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let parsed = GitUrl::parse(url).ok()?;
    let host = parsed.host()?;
    let path = parsed.path();
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    if !is_github_host(host) {
        return None;
    }
    Some((host.to_ascii_lowercase(), owner.to_owned(), name.to_owned()))
}

fn is_github_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    host == GITHUB_COM || host.ends_with(".github.com") || host.starts_with("github.")
}

pub trait Environment {
    fn get(&self, name: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

pub fn read_git_config(directory: &Path) -> Option<String> {
    let git = directory.join(".git");
    if git.is_dir() {
        return std::fs::read_to_string(git.join("config")).ok();
    }
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
