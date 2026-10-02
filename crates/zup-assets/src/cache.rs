//! Skip regeneration when the source, the padding, and the requested targets
//! are unchanged.
//!
//! The key is the directory name. A hit reads the files already there. A miss
//! writes a new directory and publishes it by rename, so a reader never sees a
//! half-written result. A cache with no directory compiles without reading or
//! writing one, for a caller that must leave the project untouched.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::encode::resources_from_ico;
use crate::error::IconError;
use crate::{CompiledIcons, IconArtifact, IconFormat, IconRole, IconSource, IconTarget, compile};

const MANIFEST: &str = "manifest";

pub struct IconCache {
    root: Option<PathBuf>,
}

impl IconCache {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    /// Compile with no directory to read from or write to.
    pub fn in_memory() -> Self {
        Self { root: None }
    }

    pub fn compile(
        &self,
        source: &IconSource<'_>,
        targets: &[IconTarget],
    ) -> Result<CompiledIcons, IconError> {
        let Some(root) = &self.root else {
            return compile(source, targets);
        };
        let key = cache_key(source, targets)?;
        let dir = root.join(&key);
        if let Some(hit) = load(&dir)? {
            return Ok(hit);
        }
        let compiled = compile(source, targets)?;
        let partial = root.join(format!("{key}.partial"));
        if partial.exists() {
            fs::remove_dir_all(&partial).map_err(|error| {
                IconError::Cache(format!("clear {}: {error}", partial.display()))
            })?;
        }
        write_dir(&partial, &compiled)?;
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .map_err(|error| IconError::Cache(format!("replace {}: {error}", dir.display())))?;
        }
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                IconError::Cache(format!("create {}: {error}", parent.display()))
            })?;
        }
        fs::rename(&partial, &dir)
            .map_err(|error| IconError::Cache(format!("publish {}: {error}", dir.display())))?;
        load(&dir)?.ok_or_else(|| IconError::Cache(format!("{} did not publish", dir.display())))
    }
}

fn cache_key(source: &IconSource<'_>, targets: &[IconTarget]) -> Result<String, IconError> {
    let padding = crate::quantize_padding(source.padding)?;
    let mut parts = targets.iter().map(IconTarget::key).collect::<Vec<_>>();
    parts.sort();
    parts.dedup();
    let mut hasher = Sha256::new();
    hasher.update(b"zup-icon-v1");
    hasher.update([source.format.tag()]);
    hasher.update(padding.to_le_bytes());
    hasher.update((source.bytes.len() as u64).to_le_bytes());
    hasher.update(source.bytes);
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    Ok(hex(&hasher.finalize()))
}

fn write_dir(dir: &Path, compiled: &CompiledIcons) -> Result<(), IconError> {
    fs::create_dir_all(dir)
        .map_err(|error| IconError::Cache(format!("create {}: {error}", dir.display())))?;
    let mut manifest = String::from("1\n");
    for warning in &compiled.warnings {
        manifest.push_str("warning ");
        manifest.push_str(&warning.replace('\n', " "));
        manifest.push('\n');
    }
    manifest.push_str("---\n");
    for artifact in &compiled.artifacts {
        let path = dir.join(artifact.name.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                IconError::Cache(format!("create {}: {error}", parent.display()))
            })?;
        }
        fs::write(&path, &artifact.bytes)
            .map_err(|error| IconError::Cache(format!("write {}: {error}", path.display())))?;
        manifest.push_str(&format!(
            "{}\t{}\t{}\n",
            role_token(&artifact.role),
            artifact.name,
            hex(&Sha256::digest(&artifact.bytes))
        ));
    }
    fs::write(dir.join(MANIFEST), manifest)
        .map_err(|error| IconError::Cache(format!("write manifest: {error}")))
}

fn load(dir: &Path) -> Result<Option<CompiledIcons>, IconError> {
    let manifest_path = dir.join(MANIFEST);
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&manifest_path)
        .map_err(|error| IconError::Cache(format!("read {}: {error}", manifest_path.display())))?;
    let mut lines = text.lines();
    if lines.next() != Some("1") {
        return Ok(None);
    }
    let mut warnings = Vec::new();
    let mut artifacts = Vec::new();
    let mut files = false;
    for line in lines {
        if line == "---" {
            files = true;
            continue;
        }
        if !files {
            if let Some(warning) = line.strip_prefix("warning ") {
                warnings.push(warning.to_owned());
            }
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let Some(role) = fields.next().and_then(parse_role) else {
            return Ok(None);
        };
        let Some(name) = fields.next() else {
            return Ok(None);
        };
        let Some(digest) = fields.next() else {
            return Ok(None);
        };
        if name.contains("..") || name.starts_with('/') || name.contains('\\') {
            return Ok(None);
        }
        let path = dir.join(name.replace('/', std::path::MAIN_SEPARATOR_STR));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        if hex(&Sha256::digest(&bytes)) != digest {
            return Ok(None);
        }
        let executable = if role == IconRole::Windows {
            Some(resources_from_ico(&bytes)?)
        } else {
            None
        };
        artifacts.push(IconArtifact {
            role,
            name: name.to_owned(),
            path,
            bytes,
            executable,
        });
    }
    if !files {
        return Ok(None);
    }
    Ok(Some(CompiledIcons {
        artifacts,
        warnings,
    }))
}

fn role_token(role: &IconRole) -> String {
    match role {
        IconRole::Windows => "windows".to_owned(),
        IconRole::MacOs => "macos".to_owned(),
        IconRole::LinuxSvg => "linux-svg".to_owned(),
        IconRole::LinuxPng { size } => format!("linux-png:{size}"),
        IconRole::Png { size } => format!("png:{size}"),
    }
}

fn parse_role(token: &str) -> Option<IconRole> {
    if let Some(size) = token.strip_prefix("linux-png:") {
        return Some(IconRole::LinuxPng {
            size: size.parse().ok()?,
        });
    }
    if let Some(size) = token.strip_prefix("png:") {
        return Some(IconRole::Png {
            size: size.parse().ok()?,
        });
    }
    match token {
        "windows" => Some(IconRole::Windows),
        "macos" => Some(IconRole::MacOs),
        "linux-svg" => Some(IconRole::LinuxSvg),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

impl IconFormat {
    fn tag(self) -> u8 {
        match self {
            Self::Svg => 1,
            Self::Png => 2,
            Self::WebP => 3,
            Self::Ico => 4,
            Self::Icns => 5,
        }
    }
}
