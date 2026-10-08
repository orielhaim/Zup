use zup_acquire::RelativeContentPath;

pub const ASSET_NAME_MAX: usize = 255;

pub const DOCUMENT_PREFIX: &str = "zup-";

pub const DOCUMENT_SUFFIX: &str = ".json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentKind<'a> {
    TufMetadata {
        version_role: &'a str,
    },
    Release {
        channel: &'a str,
        version: Option<&'a str>,
    },
    Catalog {
        channel: &'a str,
    },
    Variant {
        variant: &'a str,
    },
    Package {
        variant: &'a str,
    },
}

impl DocumentKind<'_> {
    pub fn channel(&self) -> Option<&str> {
        match self {
            Self::TufMetadata { .. } | Self::Package { .. } | Self::Variant { .. } => None,
            Self::Release { channel, .. } | Self::Catalog { channel } => Some(channel),
        }
    }
}

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

pub fn document_path(name: &str) -> Result<RelativeContentPath, String> {
    let body = name
        .strip_prefix(DOCUMENT_PREFIX)
        .and_then(|body| body.strip_suffix(DOCUMENT_SUFFIX))
        .ok_or_else(|| format!("`{name}` is not a zup release document name"))?;
    let path = if let Some(version_role) = body.strip_prefix("tuf-") {
        format!("metadata/{version_role}{DOCUMENT_SUFFIX}")
    } else if let Some(rest) = body.strip_prefix("release-") {
        match rest.split_once('-') {
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

fn looks_like_version(segment: &str) -> bool {
    let Some((head, _)) = segment.split_once('.') else {
        return false;
    };
    !head.is_empty() && head.bytes().all(|byte| byte.is_ascii_digit())
}

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
