use zup_acquire::RelativeContentPath;
use zup_core::Sha256Digest;

pub fn blob_name(digest: &Sha256Digest) -> String {
    format!("zup-blob-{}.bin", digest.to_hex())
}

pub const ASSET_NAME_MAX: usize = 255;
pub const DOCUMENT_PREFIX: &str = "zup-";
pub const DOCUMENT_SUFFIX: &str = ".json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentKind<'a> {
    TufMetadata { version_role: &'a str },
    Channel { channel: &'a str },
    Content { digest: Sha256Digest },
    Package { variant: &'a str },
}

pub fn document_name(kind: &DocumentKind<'_>) -> String {
    let body = match kind {
        DocumentKind::TufMetadata { version_role } => format!("tuf-{version_role}"),
        DocumentKind::Channel { channel } => format!("release-{channel}"),
        DocumentKind::Content { digest } => format!("document-{}", digest.to_hex()),
        DocumentKind::Package { variant } => format!("package-{variant}"),
    };
    format!("{DOCUMENT_PREFIX}{body}{DOCUMENT_SUFFIX}")
}

pub fn document_path(name: &str) -> Result<RelativeContentPath, String> {
    check_asset_name(name)?;
    let body = name
        .strip_prefix(DOCUMENT_PREFIX)
        .and_then(|body| body.strip_suffix(DOCUMENT_SUFFIX))
        .ok_or_else(|| format!("`{name}` is not a zup document name"))?;
    let path = if let Some(role) = body.strip_prefix("tuf-").filter(|role| !role.is_empty()) {
        format!("metadata/{role}.json")
    } else if let Some(channel) = body.strip_prefix("release-") {
        zup_acquire::check_channel(channel).map_err(str::to_owned)?;
        zup_acquire::release_path(channel)
            .map_err(str::to_owned)?
            .to_string()
    } else if let Some(hex) = body.strip_prefix("document-") {
        let digest: Sha256Digest = hex
            .parse()
            .map_err(|_| "document name has an invalid SHA-256 digest".to_owned())?;
        if digest.to_hex() != hex {
            return Err("document name must use a lowercase digest".into());
        }
        zup_acquire::document_path(&digest).to_string()
    } else if let Some(variant) = body.strip_prefix("package-") {
        zup_acquire::check_segment(variant).map_err(str::to_owned)?;
        format!("releases/_/packages/{variant}.json")
    } else {
        return Err(format!("`{name}` is not a zup document name"));
    };
    RelativeContentPath::parse(&path).map_err(str::to_owned)
}

pub fn asset_name(path: &RelativeContentPath) -> Result<String, String> {
    let text = path.to_string();
    let kind = if let Some(role) = text
        .strip_prefix("metadata/")
        .and_then(|role| role.strip_suffix(DOCUMENT_SUFFIX))
        .filter(|role| !role.is_empty())
    {
        DocumentKind::TufMetadata { version_role: role }
    } else if let Some(hex) = text.strip_prefix("documents/sha256/") {
        let hex = hex.replace('/', "");
        let digest: Sha256Digest = hex
            .parse()
            .map_err(|_| "document path has an invalid SHA-256 digest".to_owned())?;
        if zup_acquire::document_path(&digest) != *path {
            return Err("document path is not canonical".into());
        }
        DocumentKind::Content { digest }
    } else if let Some(variant) = text
        .strip_prefix("releases/_/packages/")
        .and_then(|variant| variant.strip_suffix(DOCUMENT_SUFFIX))
    {
        zup_acquire::check_segment(variant).map_err(str::to_owned)?;
        DocumentKind::Package { variant }
    } else if let Some(channel) = text
        .strip_prefix("releases/")
        .and_then(|channel| channel.strip_suffix(DOCUMENT_SUFFIX))
    {
        zup_acquire::check_channel(channel).map_err(str::to_owned)?;
        DocumentKind::Channel { channel }
    } else {
        return Err(format!("`{text}` is not a zup document path"));
    };
    let name = document_name(&kind);
    check_asset_name(&name)?;
    Ok(name)
}

pub fn package_name(application: &str, variant: &str) -> String {
    format!("{}.zup", safe_segment(application, variant, "-"))
}

pub const SHARD_DIGITS: usize = 3;

pub fn shard_suffix(index: usize) -> String {
    format!(".{index:0width$}", width = SHARD_DIGITS)
}

pub fn shard_name(package: &str, index: usize) -> String {
    format!("{package}{}", shard_suffix(index))
}

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

pub const fn is_asset_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

pub const fn is_tag_prefix_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

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
