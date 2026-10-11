use serde::{Deserialize, Serialize};

use crate::digest::Sha256Digest;
use crate::ids::AppId;
use crate::target::TargetTriple;

pub const MAX_IDENTITY_COMPONENTS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIdentity {
    pub app_id: AppId,
    pub release: Sha256Digest,
    pub catalog: Sha256Digest,
    pub variant: String,
    pub manifest: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Sha256Digest>,
    pub version: String,
    pub target: TargetTriple,
    #[serde(default)]
    pub channel: String,
    /// A pinned installer must not silently become a later release, and this is
    #[serde(default)]
    pub pinned: bool,
    pub trust_anchor: Sha256Digest,
    #[serde(default)]
    pub frontend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("a release identity names no version")]
    Version,
    #[error("a release identity names no variant")]
    Variant,
    #[error("a release identity names more components than this build accepts")]
    TooManyComponents,
    #[error("a release identity names an invalid segment: {0}")]
    Segment(&'static str),
    #[error("a release identity names an invalid channel")]
    Channel,
}

fn check_segment(name: &str) -> Result<(), IdentityError> {
    if name.is_empty() {
        return Err(IdentityError::Variant);
    }
    if name.len() > 64 {
        return Err(IdentityError::Segment("a segment cannot exceed 64 bytes"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.')
    {
        return Err(IdentityError::Segment(
            "a segment must be ASCII letters, digits, `-`, `_`, or `.`",
        ));
    }
    if name.starts_with('.') {
        return Err(IdentityError::Segment("a segment cannot start with `.`"));
    }
    Ok(())
}

impl ReleaseIdentity {
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub fn same_release(&self, other: &Self) -> bool {
        self.release == other.release && self.catalog == other.catalog
    }

    pub fn validate(&self) -> Result<(), IdentityError> {
        if self.version.is_empty() {
            return Err(IdentityError::Version);
        }
        check_segment(&self.variant)?;
        if !self.channel.is_empty() {
            check_channel(&self.channel)?;
        }
        if self.components.len() > MAX_IDENTITY_COMPONENTS {
            return Err(IdentityError::TooManyComponents);
        }
        for component in &self.components {
            check_segment(component).map_err(|_| IdentityError::Segment("invalid component"))?;
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        self.validate()
            .map_err(|error| serde::ser::Error::custom(error.to_string()))?;
        serde_json::to_vec(self)
    }
}

fn check_channel(channel: &str) -> Result<(), IdentityError> {
    if channel.is_empty() || channel.len() > 32 {
        return Err(IdentityError::Channel);
    }
    if !channel
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(IdentityError::Channel);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ReleaseIdentity {
        ReleaseIdentity {
            app_id: AppId::new("com.example.app").expect("valid app id"),
            release: Sha256Digest::from_bytes([1; 32]),
            catalog: Sha256Digest::from_bytes([2; 32]),
            variant: "x86_64-pc-windows-msvc".to_owned(),
            manifest: Sha256Digest::from_bytes([3; 32]),
            runtime: Some(Sha256Digest::from_bytes([4; 32])),
            version: "1.4.0".to_owned(),
            target: TargetTriple::parse("x86_64-pc-windows-msvc").expect("valid target triple"),
            channel: "stable".to_owned(),
            pinned: false,
            trust_anchor: Sha256Digest::from_bytes([5; 32]),
            frontend: "gui".to_owned(),
            components: vec!["core".to_owned()],
        }
    }

    #[test]
    fn a_traversing_segment_is_refused() {
        let mut one = identity();
        one.variant = "../escape".to_owned();
        assert!(one.validate().is_err());
        let mut two = identity();
        two.channel = "Stable".to_owned();
        assert_eq!(two.validate(), Err(IdentityError::Channel));
        assert!(one.encode().is_err());
    }
}
