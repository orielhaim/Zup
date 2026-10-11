use zup_publish::format_bytes;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NotesPolicy {
    #[default]
    Generated,
    File(String),
    Text(String),
    None,
}

impl NotesPolicy {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::File(_) => "file",
            Self::Text(_) => "text",
            Self::None => "none",
        }
    }

    pub const fn writes_body(&self) -> bool {
        match self {
            Self::Generated | Self::File(_) | Self::Text(_) => true,
            Self::None => false,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "generated" | "github" => Some(Self::Generated),
            "none" | "" => Some(Self::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    pub name: String,
    pub size: u64,
    pub role: String,
}

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
