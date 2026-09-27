//! Reading a built artifact.
//!
//! One parser, three consumers: this command, the dispatcher, and the runtime.
//! A format rule implemented here and again elsewhere is a rule that will drift,
//! so inspection reads the artifact exactly the way the machine that has to
//! install it does.

use serde::{Deserialize, Serialize};
use zup_artifact::ArtifactError;
use zup_windows::UniversalArtifact;

/// Version of the inspection report.
pub const REPORT_VERSION: u32 = 1;

/// Everything worth knowing about one artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inspection {
    /// Version of this report shape.
    pub report_version: u32,
    /// The artifact this report describes.
    pub artifact: String,
    pub application: String,
    pub application_version: String,
    pub kind: String,
    pub mode: String,
    /// Whether the artifact always installs one version or follows a channel.
    pub pin: String,
    /// The launcher experience this artifact presents.
    pub subsystem: String,
    pub variants: Vec<InspectedVariant>,
    pub content: InspectedContent,
    pub trust: InspectedTrust,
}

/// One variant inside an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedVariant {
    pub id: String,
    pub target: String,
    pub frontend: String,
    /// The size this variant's content is worth, counting shared bytes.
    pub logical_size: u64,
    pub file_count: u64,
    pub prerequisite_count: u64,
    pub plugin_count: u64,
    /// Whether the variant refuses to run under a compatibility layer.
    pub native_execution: bool,
}

/// What the artifact costs, and what composing it saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedContent {
    /// Every variant's content, counted once per variant.
    pub logical_size: u64,
    /// The distinct content the artifact actually stores.
    pub stored_size: u64,
    /// The distinct content before compression.
    pub content_size: u64,
    /// Bytes more than one variant needs.
    pub shared_size: u64,
    /// Bytes only one variant needs.
    pub exclusive_size: u64,
    pub unique_blob_count: u64,
    /// Bytes the file on disk occupies.
    pub file_size: u64,
}

/// What can be proven about an artifact's trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedTrust {
    /// Whether the image carries an Authenticode signature.
    pub authenticode: String,
    /// Whether the index parses and agrees with the content beside it.
    pub index: String,
    /// Whether every advertised content digest verifies.
    pub content_digests: String,
    /// Whether every variant the index names is complete.
    pub variants: String,
}

/// Failures produced while inspecting.
#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    Universal(#[from] zup_windows::UniversalError),
    #[error(transparent)]
    Portable(#[from] zup_pe::PeError),
    #[error("artifact I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Read an artifact and report what it contains.
///
/// Every content digest is verified, so a report never describes content the
/// artifact cannot actually produce.
pub fn inspect(path: &std::path::Path) -> Result<Inspection, InspectError> {
    let artifact = UniversalArtifact::open(path)?;
    let index = artifact.index();
    let table = artifact.view().table();
    let store = artifact.view().store();

    let mut shared_size = 0u64;
    let mut exclusive_size = 0u64;
    let mut counts: std::collections::BTreeMap<zup_core::Sha256Digest, usize> =
        std::collections::BTreeMap::new();
    for variant in &index.variants {
        let manifest = artifact.view().variant_manifest(&variant.id)?;
        for digest in manifest.content_digests() {
            *counts.entry(digest).or_default() += 1;
        }
    }
    for (digest, references) in &counts {
        let size = table.entry(digest).map_or(0, |entry| entry.size);
        if *references > 1 {
            shared_size = shared_size.saturating_add(size);
        } else {
            exclusive_size = exclusive_size.saturating_add(size);
        }
    }
    store.verify_all()?;

    let variants = index
        .variants
        .iter()
        .map(|variant| InspectedVariant {
            id: variant.id.clone(),
            target: variant.target.to_string(),
            frontend: variant.frontend.as_str().to_owned(),
            logical_size: variant.logical_size,
            file_count: variant.content.file_count,
            prerequisite_count: variant.content.prerequisite_count,
            plugin_count: variant.content.plugin_count,
            native_execution: variant.requirements.native_execution
                || !variant.requirements.capabilities.is_empty(),
        })
        .collect();

    let authenticode = if zup_pe::is_signed(path)? {
        "signed"
    } else {
        "unsigned"
    };
    let file_size = std::fs::metadata(path)?.len();
    Ok(Inspection {
        report_version: REPORT_VERSION,
        artifact: path.display().to_string(),
        application: index.artifact.application.name.to_string(),
        application_version: index.artifact.application.version.to_string(),
        kind: index.artifact.kind.as_str().to_owned(),
        mode: index.artifact.mode.as_str().to_owned(),
        pin: index.artifact.pin.label(),
        subsystem: index.artifact.subsystem.as_str().to_owned(),
        variants,
        content: InspectedContent {
            logical_size: index.standalone_size(),
            stored_size: table.stored_size(),
            content_size: table.logical_size(),
            shared_size,
            exclusive_size,
            unique_blob_count: counts.len() as u64,
            file_size,
        },
        trust: InspectedTrust {
            authenticode: authenticode.to_owned(),
            index: "valid".to_owned(),
            content_digests: "valid".to_owned(),
            variants: "valid".to_owned(),
        },
    })
}

impl Inspection {
    /// The readable view.
    pub fn human(&self) -> String {
        use zup_presentation::format_bytes;
        let mut out = String::new();
        out.push_str(&format!(
            "{} {}\n",
            self.application, self.application_version
        ));
        let kind = match (self.kind.as_str(), self.mode.as_str()) {
            ("universal", "offline") => "Windows universal offline installer".to_owned(),
            ("universal", "thin") => "Windows universal bootstrapper".to_owned(),
            ("single", "offline") => "Windows installer".to_owned(),
            ("single", "thin") => "Windows bootstrapper".to_owned(),
            (kind, mode) => format!("{kind} {mode} artifact"),
        };
        out.push_str(&kind);
        out.push_str(&format!("\n  pinned to    {}\n", self.pin));
        out.push_str("\nVariants\n");
        for variant in &self.variants {
            out.push_str(&format!(
                "  {:<24} {:<7}{}\n",
                variant.target,
                variant.frontend,
                if variant.native_execution {
                    "native only"
                } else {
                    "emulation allowed"
                }
            ));
        }
        out.push_str("\nContent\n");
        out.push_str(&format!(
            "  logical payload          {}\n",
            format_bytes(self.content.logical_size)
        ));
        out.push_str(&format!(
            "  stored                   {}\n",
            format_bytes(self.content.stored_size)
        ));
        out.push_str(&format!(
            "  deduplicated             {}\n",
            format_bytes(self.content.shared_size)
        ));
        out.push_str(&format!(
            "  unique blobs             {}\n",
            self.content.unique_blob_count
        ));
        out.push_str(&format!(
            "  file                     {}\n",
            format_bytes(self.content.file_size)
        ));
        out.push_str("\nTrust\n");
        out.push_str(&format!(
            "  Authenticode             {}\n",
            self.trust.authenticode
        ));
        out.push_str(&format!(
            "  artifact index           {}\n",
            self.trust.index
        ));
        out.push_str(&format!(
            "  content digests          {}\n",
            self.trust.content_digests
        ));
        out.push_str(&format!(
            "  variants                {}\n",
            self.trust.variants
        ));
        out
    }
}
