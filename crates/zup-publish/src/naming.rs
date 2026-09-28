//! What a release calls its files.
//!
//! # The rule
//!
//! A release asset name is the identity of a published file, and a host is free
//! to normalise a name it does not like. So two rules hold for every generated
//! name:
//!
//! 1. **Conservative characters.** `A-Z a-z 0-9 . _ -`, nothing else, no leading
//!    dot, no reserved device name. This is the intersection of what every
//!    release host, every filesystem, and every URL path segment accept, chosen
//!    so publication identity never depends on a host's discretion.
//! 2. **Derived, never authored.** A name is computed from what the file *is* -
//!    its role and the thing it describes - so two builds of the same release
//!    produce the same names, and a manifest can be checked against a host
//!    without anybody keeping a list.
//!
//! # Documents
//!
//! Release *documents* - the release descriptor, a content catalog, a variant
//! manifest, signed metadata - live in a tree with `/`-separated paths, because
//! that is what a static origin serves and what TUF target names look like. A
//! release host that accepts a flat asset name cannot serve those paths
//! directly, so each document has a flat name derived from its path, under a
//! `zup-` prefix that reserves the namespace against an installer that happens
//! to be called something similar.
//!
//! ```text
//! metadata/1.root.json                    zup-tuf-1.root.json
//! releases/stable.json                    zup-release-stable.json
//! releases/stable/versions/1.4.0.json     zup-release-stable-1.4.0.json
//! releases/stable/catalog.json            zup-catalog-stable.json
//! releases/stable/variants/win-x64.json   zup-variant-win-x64.json
//! ```
//!
//! The mapping is a bijection, which is what lets a content source recover a
//! document's tree path from the name a host reports without a side table.

use zup_acquire::RelativeContentPath;

/// The longest asset name a host is asked to hold.
pub const ASSET_NAME_MAX: usize = 255;

/// The prefix every generated document name carries.
///
/// It reserves zup's own documents against a project file that happens to share
/// a name, which is the only thing a prefix is good for.
pub const DOCUMENT_PREFIX: &str = "zup-";

/// The suffix every generated document name carries.
pub const DOCUMENT_SUFFIX: &str = ".json";

/// Which document in a release tree a flat name refers to.
///
/// Typed rather than string-matched so a change to a path shape is a change to
/// one enum, and so [`document_path`] can be written as the inverse of
/// [`document_name`] instead of a second parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentKind<'a> {
    /// Signed repository metadata, as `metadata/<version>.<role>.json`.
    ///
    /// `version_role` is the part after `metadata/` and before `.json`, such as
    /// `1.root` or `7.timestamp`.
    TufMetadata { version_role: &'a str },
    /// The release descriptor for a channel, or for one version of it.
    Release {
        channel: &'a str,
        version: Option<&'a str>,
    },
    /// The content catalog for a channel.
    Catalog { channel: &'a str },
    /// One variant's install plan.
    Variant { variant: &'a str },
    /// One transport package's descriptor, which is what makes a sharded
    /// package one logical source rather than several files.
    Package { variant: &'a str },
}

impl DocumentKind<'_> {
    /// The channel this document belongs to, when it has one.
    pub fn channel(&self) -> Option<&str> {
        match self {
            Self::TufMetadata { .. } | Self::Package { .. } | Self::Variant { .. } => None,
            Self::Release { channel, .. } | Self::Catalog { channel } => Some(channel),
        }
    }
}

/// The flat asset name for a document.
pub fn document_name(kind: &DocumentKind<'_>) -> String {
    let body = match kind {
        DocumentKind::TufMetadata { version_role } => format!("tuf-{version_role}"),
        DocumentKind::Release {
            channel,
            version: None,
        } => format!("release-{channel}"),
        DocumentKind::Release {
            channel,
            version: Some(version),
        } => format!("release-{channel}-{version}"),
        DocumentKind::Catalog { channel } => format!("catalog-{channel}"),
        DocumentKind::Variant { variant } => format!("variant-{variant}"),
        DocumentKind::Package { variant } => format!("package-{variant}"),
    };
    format!("{DOCUMENT_PREFIX}{body}{DOCUMENT_SUFFIX}")
}

/// The tree path a document lives at, which is the inverse of [`document_name`].
///
/// A path is only produced for documents that address a channel. Signed metadata
/// is addressed by role and version rather than by channel, and its tree path is
/// whatever a repository's own layout says, so this refuses rather than guesses.
pub fn document_path(name: &str) -> Result<RelativeContentPath, String> {
    let body = name
        .strip_prefix(DOCUMENT_PREFIX)
        .and_then(|body| body.strip_suffix(DOCUMENT_SUFFIX))
        .ok_or_else(|| format!("`{name}` is not a zup release document name"))?;
    let path = if let Some(version_role) = body.strip_prefix("tuf-") {
        format!("metadata/{version_role}{DOCUMENT_SUFFIX}")
    } else if let Some(rest) = body.strip_prefix("release-") {
        match rest.split_once('-') {
            // A channel may itself contain a `-`, so the version is only the
            // final `-`-separated segment when it looks like one.
            Some((channel, version)) if looks_like_version(version) => {
                format!("releases/{channel}/versions/{version}{DOCUMENT_SUFFIX}")
            }
            _ => format!("releases/{rest}{DOCUMENT_SUFFIX}"),
        }
    } else if let Some(channel) = body.strip_prefix("catalog-") {
        format!("releases/{channel}/catalog{DOCUMENT_SUFFIX}")
    } else if let Some(variant) = body.strip_prefix("variant-") {
        format!("releases/_/variants/{variant}{DOCUMENT_SUFFIX}")
    } else if let Some(variant) = body.strip_prefix("package-") {
        format!("releases/_/packages/{variant}{DOCUMENT_SUFFIX}")
    } else {
        return Err(format!("`{name}` is not a zup release document name"));
    };
    RelativeContentPath::parse(&path).map_err(|reason| reason.to_owned())
}

/// Whether a trailing segment is a version rather than part of a channel name.
fn looks_like_version(segment: &str) -> bool {
    let Some((head, _)) = segment.split_once('.') else {
        return false;
    };
    !head.is_empty() && head.bytes().all(|byte| byte.is_ascii_digit())
}

/// The flat asset name for any path in a release tree.
///
/// A blob path resolves to nothing: a release never carries one file per blob.
/// That refusal is the whole point of a content package, and a publisher that
/// tried to flatten it would produce a release with ten thousand assets.
pub fn asset_name(path: &RelativeContentPath) -> Result<String, String> {
    let path = path.to_string();
    if let Some(rest) = path.strip_prefix("metadata/") {
        if let Some(version_role) = rest.strip_suffix(DOCUMENT_SUFFIX) {
            return Ok(document_name(&DocumentKind::TufMetadata { version_role }));
        }
        return Err(format!("`{path}` is not a zup release document"));
    }
    if let Some(rest) = path.strip_prefix("releases/") {
        if let Some(channel) = rest.strip_suffix(DOCUMENT_SUFFIX) {
            return Ok(document_name(&DocumentKind::Release {
                channel,
                version: None,
            }));
        }
        let Some((channel, tail)) = rest.split_once('/') else {
            return Err(format!("`{path}` is not a zup release document"));
        };
        if let Some(version) = tail
            .strip_prefix("versions/")
            .and_then(|v| v.strip_suffix(DOCUMENT_SUFFIX))
        {
            return Ok(document_name(&DocumentKind::Release {
                channel,
                version: Some(version),
            }));
        }
        if tail == format!("catalog{DOCUMENT_SUFFIX}") {
            return Ok(document_name(&DocumentKind::Catalog { channel }));
        }
        if let Some(variant) = tail
            .strip_prefix("variants/")
            .and_then(|v| v.strip_suffix(DOCUMENT_SUFFIX))
        {
            return Ok(document_name(&DocumentKind::Variant { variant }));
        }
        if let Some(variant) = tail
            .strip_prefix("packages/")
            .and_then(|v| v.strip_suffix(DOCUMENT_SUFFIX))
        {
            return Ok(document_name(&DocumentKind::Package { variant }));
        }
    }
    if path.starts_with("blobs/") {
        return Err(format!(
            "`{path}` is one content object; a release publishes content as packages, \
             not as one asset per object"
        ));
    }
    Err(format!("`{path}` is not a zup release document"))
}

/// The name of a variant's transport package.
///
/// `.zup` rather than a second installer extension, because the file is a
/// container the acquisition engine reads and never a program anyone runs.
pub fn package_name(application: &str, variant: &str) -> String {
    format!("{}.zup", safe_segment(application, variant, "-"))
}

/// The suffix a sharded package piece carries.
///
/// A three-digit suffix sorts correctly as a string up to a thousand pieces,
/// which is four terabytes of transport at GitHub's per-asset limit - far past
/// the point where a different layout would matter, and a layout that changes is
/// a format version, not a silent widening.
pub const SHARD_DIGITS: usize = 3;

/// The suffix of shard `index`.
pub fn shard_suffix(index: usize) -> String {
    format!(".{index:0width$}", width = SHARD_DIGITS)
}

/// The name of shard `index` of `package`.
pub fn shard_name(package: &str, index: usize) -> String {
    format!("{package}{}", shard_suffix(index))
}

/// The index a shard name carries, if it is one.
pub fn shard_index(shard: &str) -> Option<usize> {
    let (package, suffix) = shard.rsplit_once('.')?;
    if package.is_empty()
        || suffix.len() != SHARD_DIGITS
        || !suffix.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    suffix.parse().ok()
}

/// Whether a name survives every host, filesystem, and URL path segment.
///
/// The reserved device names are the part that is easy to forget: a file called
/// `aux` or `nul` is fine in a release manifest and unusable on a Windows
/// machine, and the machine that has to write it is often the one that has to
/// cache it.
pub fn check_asset_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a name cannot be empty".to_owned());
    }
    if name.len() > ASSET_NAME_MAX {
        return Err(format!(
            "a name may be at most {ASSET_NAME_MAX} bytes, and this one is {}",
            name.len()
        ));
    }
    if name.starts_with('.') {
        return Err("a name may not begin with a dot".to_owned());
    }
    if name.ends_with('.') {
        return Err("a name may not end with a dot".to_owned());
    }
    if !name.bytes().all(is_asset_name_byte) {
        let offending = name
            .chars()
            .find(|character| !is_asset_name_byte(*character as u8))
            .map(|character| format!("`{character}`"))
            .unwrap_or_else(|| "a non-ascii character".to_owned());
        return Err(format!(
            "a name may use only letters, digits, `.`, `_`, and `-`; it contains {offending}"
        ));
    }
    let stem = name.split('.').next().unwrap_or(name);
    const RESERVED: [&str; 22] = [
        "aux", "clock$", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
        "con", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "nul",
    ];
    if RESERVED.contains(&stem.to_ascii_lowercase().as_str()) {
        return Err(format!("`{stem}` is a reserved device name"));
    }
    Ok(())
}

/// Whether a byte is inside the conservative asset-name set.
pub const fn is_asset_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Whether a byte may appear in a tag prefix.
///
/// A tag is spelled into a URL path, a shell command, a ref, and an asset name,
/// so the prefix is held to the same conservative set as everything else.
pub const fn is_tag_prefix_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Reduce a string to the conservative character set, deterministically.
///
/// Used where a project's own name is the input, such as an application called
/// `Acme (beta)`. The mapping is fixed and lossy, because the alternative is a
/// name the host silently rewrites, and a release manifest that disagrees with
/// the host about a filename is a release nobody can verify.
pub fn safe_segment(value: &str, secondary: &str, join: &str) -> String {
    let mut out = String::with_capacity(value.len() + secondary.len() + join.len());
    let mut last_was_join = true;
    for source in [value, join, secondary] {
        for character in source.chars() {
            if is_asset_name_byte(character as u8) && character.is_ascii() {
                if last_was_join && (character == '.' || character == '-') {
                    continue;
                }
                out.push(character);
                last_was_join = false;
            } else if !last_was_join {
                out.push('-');
                last_was_join = true;
            }
        }
    }
    while out.ends_with('-') || out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("release");
    }
    out
}
