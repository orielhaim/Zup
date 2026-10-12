use zup_publish::PlanError;

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("github endpoint: {reason}")]
    Host { reason: String },

    #[error("github repository: {reason}")]
    Repository { reason: String },

    #[error(
        "no GitHub repository could be determined; pass --repo owner/name, set \
         `[publish.github] repository`, set GITHUB_REPOSITORY, or add a GitHub remote"
    )]
    NoRepository,

    #[error(
        "several GitHub remotes are configured ({}) and none is named `origin`; \
         pass --repo owner/name to say which repository this release belongs to",
        candidates.join(", ")
    )]
    AmbiguousRepository { candidates: Vec<String> },

    #[error(
        "no GitHub credential is available; set GH_TOKEN or GITHUB_TOKEN, or run `gh auth \
         login` so `gh auth token` can supply one. A token is never stored in zup.toml."
    )]
    NoToken,

    #[error("github refused the credential: {reason}")]
    Unauthenticated { reason: String },

    #[error("github could not be reached: {reason}")]
    Transport { reason: String },

    #[error("github answered {status}{}", detail_suffix(.message))]
    Status {
        status: u16,
        message: Option<String>,
    },

    #[error("github's rate limit for this token is exhausted; retry in {}s", seconds.unwrap_or(0))]
    RateLimited { seconds: Option<u64> },

    #[error("{what} does not exist")]
    NotFound { what: String },

    #[error(
        "tag `{tag}` does not exist; a release must be published for a commit that was tagged, \
         not for whichever commit a branch happens to point at. Create the tag and push it, or \
         opt in to having the publisher create it."
    )]
    MissingTag { tag: String },

    #[error("`{name}`: the release expects sha256:{expected} and github reports sha256:{found}")]
    Digest {
        name: String,
        expected: String,
        found: String,
    },

    #[error("`{name}`: the release expects {expected} bytes and github reports {found}")]
    Size {
        name: String,
        expected: u64,
        found: u64,
    },

    #[error("`{expected}` was uploaded and github reports it as `{found}`")]
    Renamed { expected: String, found: String },

    #[error(
        "`{tag}` is already published and its assets differ from this release plan; a published \
         release is not a retry target. Bump the version, or delete the release by hand."
    )]
    PublishedConflict { tag: String },

    #[error("the draft release is `{state}`: {reason}")]
    DraftState { state: String, reason: String },

    #[error(transparent)]
    Plan(#[from] PlanError),

    #[error("`{path}`: {reason}")]
    Io { path: String, reason: String },

    #[error("github's answer could not be read: {reason}")]
    Decode { reason: String },
}

impl GithubError {
    pub fn status(status: u16, message: Option<String>) -> Self {
        Self::Status { status, message }
    }

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
