use std::fmt;
use std::process::Command;

use secrecy::{ExposeSecret, SecretString};

use crate::error::GithubError;
use crate::repository::Environment;

#[derive(Clone)]
pub struct Token {
    value: SecretString,
    source: Source,
}

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: SecretString::from(value.into()),
            source: Source::Supplied,
        }
    }

    pub fn expose(&self) -> &str {
        self.value.expose_secret()
    }

    pub fn source(&self) -> &'static str {
        self.source.as_str()
    }

    pub fn is_ambient(&self) -> bool {
        self.source == Source::Environment
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Environment,
    Gh,
    Supplied,
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Token({:?}, from {})",
            self.value,
            self.source.as_str()
        )
    }
}

impl Token {
    pub fn same_secret_as(&self, other: &Self) -> bool {
        let (left, right) = (self.expose().as_bytes(), other.expose().as_bytes());
        if left.len() != right.len() {
            return false;
        }
        left.iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
    }
}

impl Source {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "the environment",
            Self::Gh => "`gh auth token`",
            Self::Supplied => "the caller",
        }
    }
}

pub fn discover(environ: &dyn Environment) -> Result<Token, GithubError> {
    discover_with(environ, gh_auth_token)
}

pub fn discover_with(
    environ: &dyn Environment,
    fallback: impl FnOnce() -> Option<String>,
) -> Result<Token, GithubError> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Some(value) = non_empty(environ, name) {
            return Ok(Token {
                value: SecretString::from(value),
                source: Source::Environment,
            });
        }
    }
    match fallback() {
        Some(value) => Ok(Token {
            value: SecretString::from(value),
            source: Source::Gh,
        }),
        None => Err(GithubError::NoToken),
    }
}

pub fn supplied(value: impl Into<String>) -> Token {
    Token::new(value)
}

fn non_empty(environ: &dyn Environment, name: &str) -> Option<String> {
    environ
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn gh_auth_token() -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

pub fn authorization(token: &Token) -> String {
    format!("Bearer {}", token.expose())
}
