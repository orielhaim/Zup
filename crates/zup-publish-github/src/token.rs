//! Where a GitHub credential comes from, and where it must never go.
//!
//! # Discovery
//!
//! ```text
//! GH_TOKEN        ┐
//! GITHUB_TOKEN    ├─ in that order, and then
//! `gh auth token` ┘   only if the environment had nothing
//! ```
//!
//! `gh auth token` is a subprocess and therefore a last resort: a convenience for
//! a developer who has already authenticated `gh`, skipped entirely when an
//! environment token exists so a CI job is never slowed by a process spawn or
//! surprised by an interactive helper.
//!
//! # Where a token is never
//!
//! Not in `zup.toml`. Not in a release manifest. Not in a receipt. Not in a
//! `Debug` output, a log line, a retry message, or a panic.
//!
//! Those are not stylistic rules. A manifest is committed and a receipt is
//! uploaded as a build output; a token in either one reaches every fork, every
//! mirror, and every issue somebody pastes a diagnostic into.
//!
//! # Why `secrecy`
//!
//! A hand-written `Debug` impl is one derive away from not working:
//! `#[derive(Debug)]` on any struct holding a `Token` prints the value, and
//! nothing in the type system objects. `SecretString` makes that impossible — its
//! `Debug` prints `[REDACTED]`, it has no `Display`, and the only way out is
//! [`ExposeSecret::expose_secret`]. It has no dependencies of its own.
//!
//! Redacting `Debug` says nothing about the wire, so the `Authorization` header is
//! still built by hand and still marked sensitive.

use std::fmt;
use std::process::Command;

use secrecy::{ExposeSecret, SecretString};

use crate::error::GithubError;
use crate::repository::Environment;

/// A credential that must not reach a diagnostic.
///
/// The value lives in a [`SecretString`], so leaking it is not a matter of
/// remembering to be careful: the value is reachable only through
/// [`expose`](Token::expose), which reads like the dangerous thing it is at every
/// call site.
#[derive(Clone)]
pub struct Token {
    value: SecretString,
    source: Source,
}

impl Token {
    /// Wrap a token value.
    ///
    /// Public because a caller may have a token from a source this crate does
    /// not know about, such as a test harness or a corporate wrapper. The
    /// redaction, not the acquisition, is what this type is for.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: SecretString::from(value.into()),
            source: Source::Supplied,
        }
    }

    /// The value, for a request builder.
    ///
    /// Named after the unsafe thing it is: every call site is a place a token
    /// could reach a log.
    pub fn expose(&self) -> &str {
        self.value.expose_secret()
    }

    /// Where this token came from.
    pub fn source(&self) -> &'static str {
        self.source.as_str()
    }

    /// Whether this is the job-scoped token Actions provides.
    ///
    /// Worth knowing because its permissions are whatever the workflow granted,
    /// which is usually narrower than a personal token's and is a better
    /// configuration rather than a limitation to work around.
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

/// A credential that redacts itself, adding the one fact worth knowing while
/// debugging a failed upload: where it came from.
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

/// Deliberately not `PartialEq`: two tokens comparing equal exactly when their
/// secrets match is a convenience that would let a test assert on a credential it
/// is trying to keep out of output.
impl Token {
    /// Whether two tokens hold the same value. Constant-time, because a timing
    /// side channel on a secret is still a leak.
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

/// Find a credential, or say where one would go.
pub fn discover(environ: &dyn Environment) -> Result<Token, GithubError> {
    discover_with(environ, gh_auth_token)
}

/// Find a credential, with the `gh` fallback supplied by the caller.
///
/// The fallback is a parameter because it is the only part of discovery that is
/// not a pure function of the environment — it shells out — and a test that
/// cannot control it is a test whose result depends on whether the developer has
/// the `gh` CLI installed.
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

/// A token supplied by a caller, for tests and for hosts that manage their own.
pub fn supplied(value: impl Into<String>) -> Token {
    Token::new(value)
}

fn non_empty(environ: &dyn Environment, name: &str) -> Option<String> {
    environ
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Ask the `gh` CLI for a token.
///
/// Two deliberate restrictions. The command is given no arguments that could make
/// it interactive, and its output is read from a pipe rather than from a
/// terminal, so a developer without `gh` installed gets a normal command-not-found
/// failure instead of a hang. A token that fails to be found is `None`, never an
/// error: it is a fallback, and a fallback that can fail the operation is not a
/// fallback.
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

/// The `Authorization` header value for a token.
///
/// `Bearer` rather than the older `token` scheme. Both are accepted, but
/// `Bearer` is what the documentation specifies for a fine-grained token and it
/// is the scheme that keeps working if a future credential is not a classic
/// personal access token at all.
pub fn authorization(token: &Token) -> String {
    format!("Bearer {}", token.expose())
}
