//! Why a GitHub publication did not finish.
//!
//! Every variant names a class of failure a developer can act on, and none of
//! them names a host identifier, an API path, or a credential. Two reasons: a
//! diagnostic gets copied into an issue, and an issue is public; and a
//! `reqwest` error's `Display` contains the URL, which for an authenticated
//! request is a place a token was on its way to being leaked.

use zup_publish::PlanError;

/// Why a GitHub publication failed.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// The installation's addresses could not be built.
    #[error("github endpoint: {reason}")]
    Host { reason: String },

    /// The repository could not be named.
    #[error("github repository: {reason}")]
    Repository { reason: String },

    /// No GitHub remote is configured and nothing else named one.
    ///
    /// The three sources are listed in the order they are consulted, because a
    /// refusal that omits one of them sends the reader looking in a place that
    /// would not have worked anyway.
    #[error(
        "no GitHub repository could be determined; pass --repo owner/name, set \
         `[publish.github] repository`, set GITHUB_REPOSITORY, or add a GitHub remote"
    )]
    NoRepository,

    /// More than one GitHub remote is configured and none is obviously the one.
    #[error(
        "several GitHub remotes are configured ({}) and none is named `origin`; \
         pass --repo owner/name to say which repository this release belongs to",
        candidates.join(", ")
    )]
    AmbiguousRepository { candidates: Vec<String> },

    /// No credential was found.
    #[error(
        "no GitHub credential is available; set GH_TOKEN or GITHUB_TOKEN, or run `gh auth \
         login` so `gh auth token` can supply one. A token is never stored in zup.toml."
    )]
    NoToken,

    /// A credential was found but rejected.
    #[error("github refused the credential: {reason}")]
    Unauthenticated { reason: String },

    /// The request never completed.
    #[error("github could not be reached: {reason}")]
    Transport { reason: String },

    /// The API answered with a status this operation cannot use.
    #[error("github answered {status}{}", detail_suffix(.message))]
    Status {
        status: u16,
        message: Option<String>,
    },

    /// The API rate limit is exhausted.
    #[error("github's rate limit for this token is exhausted; retry in {}s", seconds.unwrap_or(0))]
    RateLimited { seconds: Option<u64> },

    /// The requested thing is not there.
    #[error("{what} does not exist")]
    NotFound { what: String },

    /// The tag the release names does not exist and may not be created.
    #[error(
        "tag `{tag}` does not exist; a release must be published for a commit that was tagged, \
         not for whichever commit a branch happens to point at. Create the tag and push it, or \
         opt in to having the publisher create it."
    )]
    MissingTag { tag: String },

    /// A remote asset's digest is not the one the plan names.
    #[error("`{name}`: the release expects sha256:{expected} and github reports sha256:{found}")]
    Digest {
        name: String,
        expected: String,
        found: String,
    },

    /// A remote asset's size is not the one the plan names.
    #[error("`{name}`: the release expects {expected} bytes and github reports {found}")]
    Size {
        name: String,
        expected: u64,
        found: u64,
    },

    /// The host reported a different name than the one uploaded.
    #[error("`{expected}` was uploaded and github reports it as `{found}`")]
    Renamed { expected: String, found: String },

    /// A published release would have to change.
    #[error(
        "`{tag}` is already published and its assets differ from this release plan; a published \
         release is not a retry target. Bump the version, or delete the release by hand."
    )]
    PublishedConflict { tag: String },

    /// The draft is in a state this operation cannot continue from.
    #[error("the draft release is `{state}`: {reason}")]
    DraftState { state: String, reason: String },

    /// The plan itself is not publishable.
    #[error(transparent)]
    Plan(#[from] PlanError),

    /// A local file could not be read.
    #[error("`{path}`: {reason}")]
    Io { path: String, reason: String },

    /// A response could not be understood.
    #[error("github's answer could not be read: {reason}")]
    Decode { reason: String },
}

impl GithubError {
    /// A `Status` for `status`, with whatever message the API sent.
    ///
    /// The message is GitHub's own error text, which names a field or a
    /// constraint and never a credential, and it is the difference between "it
    /// failed" and "this name is already taken".
    pub fn status(status: u16, message: Option<String>) -> Self {
        Self::Status { status, message }
    }

    /// The failure as something a plan error can carry.
    pub fn plan(self) -> PlanError {
        match self {
            Self::Plan(error) => error,
            other => PlanError::Other(other.to_string()),
        }
    }
}

fn detail_suffix(message: &Option<String>) -> String {
    match message {
        Some(message) if !message.is_empty() => format!(": {message}"),
        _ => String::new(),
    }
}
