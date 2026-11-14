use serde::{Deserialize, Serialize};

use crate::digest::Sha256Digest;
use crate::ids::AppId;
use crate::target::Target;

pub const MAX_IDENTITY_COMPONENTS: usize = 1024;

/// How a release names itself, which decides whether a bootstrapper resolves
/// it now or was built for one exact graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "pin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReleasePin {
    /// Exactly this release. A thin installer built this way always installs
    /// this graph and never silently becomes a later one.
    Release {
        version: semver::Version,
        document: Sha256Digest,
    },
    /// Whatever the channel currently resolves to, authenticated at runtime.
    Channel { channel: String },
}

impl ReleasePin {
    /// A short label for a build output and a window title.
    pub fn label(&self) -> String {
        match self {
            Self::Release { version, .. } => version.to_string(),
            Self::Channel { channel } => channel.clone(),
        }
    }

    /// Whether this pin names one graph forever.
    pub const fn is_pinned(&self) -> bool {
        matches!(self, Self::Release { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCandidate {
    pub version: semver::Version,
    pub release: Sha256Digest,
}

impl ReleaseCandidate {
    pub fn is_update_for(
        &self,
        version: &semver::Version,
        installed: Option<&ReleaseIdentity>,
    ) -> bool {
        self.version >= *version
            && installed.is_none_or(|installed| installed.release != self.release)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIdentity {
    pub app_id: AppId,
    pub release: Sha256Digest,
    pub document: Sha256Digest,
    pub catalog: Sha256Digest,
    pub variant: String,
    pub manifest: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Sha256Digest>,
    pub version: String,
    pub target: Target,
    pub channel: String,
    pub repository: String,
    pub pin: ReleasePin,
    pub trust_anchor: Sha256Digest,
    pub frontend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("authenticated content does not match the exact source recorded for this installation")]
    Source,
    #[error("release pin differs from the selected document, version, or channel")]
    Pin,
    #[error("release repository locator is empty or exceeds 2048 bytes")]
    Repository,
    #[error("a release identity needs a semantic version")]
    Version,
    #[error("a release identity names no frontend")]
    Frontend,
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
    pub fn verify_source(&self, expected: &Self) -> Result<(), IdentityError> {
        self.validate()?;
        expected.validate()?;
        // Selection provenance can change when resolving a retained source.
        let expected = Self {
            pin: self.pin.clone(),
            ..expected.clone()
        };
        if *self != expected {
            return Err(IdentityError::Source);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), IdentityError> {
        if semver::Version::parse(&self.version).is_err() {
            return Err(IdentityError::Version);
        }
        if self.frontend.is_empty() {
            return Err(IdentityError::Frontend);
        }
        check_segment(&self.variant)?;
        check_channel(&self.channel)?;
        if self.repository.is_empty() || self.repository.len() > 2048 {
            return Err(IdentityError::Repository);
        }
        if match &self.pin {
            ReleasePin::Release { version, document } => {
                version.to_string() != self.version || *document != self.document
            }
            ReleasePin::Channel { channel } => channel != &self.channel,
        } {
            return Err(IdentityError::Pin);
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
            document: Sha256Digest::from_bytes([6; 32]),
            catalog: Sha256Digest::from_bytes([2; 32]),
            variant: "windows-x64".to_owned(),
            manifest: Sha256Digest::from_bytes([3; 32]),
            runtime: Some(Sha256Digest::from_bytes([4; 32])),
            version: "1.4.0".to_owned(),
            target: Target::parse("windows-x64").expect("a valid target"),
            channel: "stable".to_owned(),
            pin: ReleasePin::Channel {
                channel: "stable".into(),
            },
            repository: "https://example.invalid".into(),
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

    #[rstest::rstest]
    #[case("1.4.0", 1, false)]
    #[case("1.4.0", 9, true)]
    #[case("1.5.0", 9, true)]
    #[case("1.3.0", 9, false)]
    fn update_selection_uses_release_content_and_version_policy(
        #[case] version: &str,
        #[case] digest: u8,
        #[case] available: bool,
    ) {
        let installed = identity();
        let candidate = ReleaseCandidate {
            version: version.parse().unwrap(),
            release: Sha256Digest::from_bytes([digest; 32]),
        };
        assert_eq!(
            candidate.is_update_for(&"1.4.0".parse().unwrap(), Some(&installed)),
            available
        );
    }
}
