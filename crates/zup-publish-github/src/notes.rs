//! What a release's notes say, and who writes them.
//!
//! # The decision this makes
//!
//! GitHub already generates release notes from the commit log, and already
//! categorises them through `.github/release.yml`. Rebuilding that inside zup
//! would mean reimplementing a changelog engine whose output a project would
//! then have to trust more than GitHub's own, and a zup-generated changelog is
//! strictly worse than no changelog: it looks authoritative and is derived from
//! commit subjects.
//!
//! So the default is GitHub's own generator, and a project that wants something
//! else names it:
//!
//! | policy    | what the body is                                              |
//! |-----------|---------------------------------------------------------------|
//! | generated | GitHub's notes, plus a zup-generated download table appended   |
//! | file      | a file in the repository, verbatim                              |
//! | text      | text the caller supplied, verbatim                              |
//! | none      | nothing, which leaves a draft that was already written alone    |
//!
//! # Never overwriting a body somebody wrote
//!
//! Two rules, both load-bearing:
//!
//! - A release that already has a body keeps it, unless the caller explicitly
//!   asked for a different one. A project that curates its release notes in a
//!   draft, or has Release Drafter filling them in, must not have that replaced
//!   by a generated changelog the first time zup publishes binaries to it.
//! - A body is only sent in a request that sets it, so an update that changes
//!   nothing else leaves the body byte-identical.
//!
//! That second rule is why the zup download table is *appended* rather than
//! *rendered*: a project that has written a changelog still gets to know which
//! file to download, without losing what it wrote.

use zup_publish::format_bytes;

/// Where a release's notes come from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NotesPolicy {
    /// GitHub generates them from the commit log.
    #[default]
    Generated,
    /// A file in the repository, read and sent verbatim.
    File(String),
    /// Text the caller supplied.
    Text(String),
    /// No notes at all.
    None,
}

impl NotesPolicy {
    /// The name a manifest and a report use.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::File(_) => "file",
            Self::Text(_) => "text",
            Self::None => "none",
        }
    }

    /// Whether this policy writes a body at all.
    ///
    /// `None` does not, and neither does `Generated` when the release already
    /// has a body: appending to something a person wrote is a decision the
    /// caller has to make, not one this policy makes for them.
    pub const fn writes_body(&self) -> bool {
        match self {
            Self::Generated | Self::File(_) | Self::Text(_) => true,
            Self::None => false,
        }
    }

    /// Parse the manifest spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "generated" | "github" => Some(Self::Generated),
            "none" | "" => Some(Self::None),
            _ => None,
        }
    }
}

/// One file the release carries, for the download table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    pub name: String,
    pub size: u64,
    /// What the file is for, as the plan described it.
    pub role: String,
}

/// The section zup appends to a generated body.
///
/// Small on purpose. A release page is read by a person deciding whether to
/// download something, and a table of every asset — including every internal
/// transport object — is noise. So the table lists what a person would install.
pub fn download_section(rows: &[DownloadRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Downloads\n\n");
    out.push_str("| File | Size | |\n");
    out.push_str("| --- | ---: | --- |\n");
    for row in rows {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            row.name,
            format_bytes(row.size),
            row.role
        ));
    }
    out.push_str("\nEvery file is verified by SHA-256 before it is installed; the digests are in `zup-release.json`.\n");
    out
}

/// Compose the body for a new release.
///
/// Returns `None` when the policy writes nothing, which is the caller's signal
/// to omit the field from the request entirely rather than to send an empty
/// string. An empty body and an absent body are different documents to a human,
/// and `zup` does not get to decide which one a project wanted.
pub fn compose(
    policy: &NotesPolicy,
    generated: Option<String>,
    file_body: Option<String>,
    text: Option<String>,
    downloads: &[DownloadRow],
) -> Option<String> {
    let base = match policy {
        NotesPolicy::None => return None,
        NotesPolicy::Generated => generated,
        NotesPolicy::File(_) => file_body,
        NotesPolicy::Text(_) => text,
    };
    let base = base.unwrap_or_default();
    let section = download_section(downloads);
    if section.is_empty() {
        return Some(base);
    }
    if base.is_empty() {
        return Some(section);
    }
    Some(format!("{}\n\n{}", base.trim_end(), section))
}
