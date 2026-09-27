//! The OCI adapter.
//!
//! The internal graph was designed to map onto an OCI image index: a small root
//! that references immutable content by digest, one manifest per platform
//! variant, and a content store addressed by the same digests. This module is
//! the proof, and it is deliberately an *adapter*:
//!
//! - OCI types appear only here. Nothing else in the crate names an OCI
//!   concept, and `zup-core` never will.
//! - The digests are the same digests. A registry stores exactly the bytes the
//!   installer verifies, so publishing to a registry and installing from a local
//!   executable are the same verification.
//! - Frontends, requirements, and lifecycle concepts stay typed, in a zup
//!   config blob the manifest points at. OCI has no field for them, and hiding
//!   them in opaque annotations would make them unverifiable.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use oci_spec::image::{
    Arch, Descriptor as OciDescriptor, Digest, ImageIndex, ImageIndexBuilder, ImageManifest,
    ImageManifestBuilder, MediaType as OciMediaType, Os, Platform as OciPlatform, PlatformBuilder,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zup_core::Sha256Digest;

/// The OCI SHA-256 digest of `bytes`, which is zup's digest over the same
/// content.
fn sha256_of(bytes: &[u8]) -> oci_spec::image::Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
        .to_hex()
        .parse()
        .expect("a lowercase hex SHA-256 digest parses as an OCI digest")
}

use crate::compose::ArtifactGraph;
use crate::descriptor::Descriptor;
use crate::error::ArtifactError;
use crate::media_type::MediaType;
use crate::platform::Platform;
use crate::variant::{VariantDescriptor, VariantRequirements};

/// OCI media type for the artifact index.
pub const OCI_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// OCI media type for a translated variant manifest.
pub const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Media type of the per-variant config blob, which carries the content a
/// container image has no field for.
pub const OCI_CONFIG_MEDIA_TYPE: &str = "application/vnd.zup.artifact.variant-config.v1+json";

/// Annotation naming the variant an index entry or manifest refers to.
pub const OCI_VARIANT_TITLE: &str = "org.opencontainers.image.title";

/// Annotation naming the artifact an index belongs to.
pub const OCI_ARTIFACT: &str = "io.zup.artifact.id";

/// OCI image index schema version.
pub const OCI_SCHEMA_VERSION: u32 = 2;

/// Current variant config schema.
pub const VARIANT_CONFIG_SCHEMA: u32 = 1;

/// The zup document stored as a variant's OCI config blob.
///
/// This is where the content model lives. A client that receives only OCI
/// metadata still learns the variant's target, frontend, requirements, and the
/// exact digests it must fetch, which is what makes the descriptor graph
/// verifiable without zup's own index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantConfig {
    pub schema: u32,
    pub id: String,
    pub target: zup_core::TargetTriple,
    pub platform: Platform,
    pub frontend: zup_core::Frontend,
    pub requirements: VariantRequirements,
    /// The zup variant manifest, which carries the installer's content plan.
    pub manifest: Descriptor,
    /// Content digests the variant requires, in ascending order.
    pub content: Vec<Sha256Digest>,
    pub logical_size: u64,
}

/// The digest string OCI uses, which is the algorithm-prefixed form of the
/// digest zup carries.
pub fn oci_digest(digest: &Sha256Digest) -> Digest {
    Digest::from(
        digest
            .to_hex()
            .parse::<oci_spec::image::Sha256Digest>()
            .expect("a lowercase hex SHA-256 digest parses as an OCI digest"),
    )
}

/// The zup digest an OCI digest names, or `None` when the algorithm is not one
/// zup verifies.
pub fn zup_digest(digest: &Digest) -> Option<Sha256Digest> {
    match digest.algorithm() {
        oci_spec::image::DigestAlgorithm::Sha256 => digest.digest().parse().ok(),
        _ => None,
    }
}

/// An OCI descriptor for a zup descriptor.
pub fn oci_descriptor(descriptor: &Descriptor, media_type: &str) -> OciDescriptor {
    OciDescriptor::new(
        OciMediaType::Other(media_type.to_owned()),
        descriptor.size,
        oci_digest(&descriptor.digest),
    )
}

/// One variant, expressed as an OCI manifest plus the config blob it needs.
#[derive(Debug, Clone)]
pub struct OciVariant {
    pub id: String,
    pub manifest: ImageManifest,
    pub config: Vec<u8>,
    pub config_descriptor: OciDescriptor,
}

/// The artifact graph, expressed as an OCI image index.
#[derive(Debug, Clone)]
pub struct OciLayout {
    pub index: ImageIndex,
    pub variants: Vec<OciVariant>,
}

/// Convert a composed graph into an OCI index and one manifest per variant.
///
/// The shared store becomes the variants' layers, so a registry deduplicates the
/// store exactly as the local artifact does: one blob, referenced by both
/// manifests.
pub fn to_oci(graph: &ArtifactGraph) -> Result<OciLayout, ArtifactError> {
    let index = graph.index();
    let mut variants = Vec::with_capacity(index.variants.len());
    let mut descriptors = Vec::with_capacity(index.variants.len());
    for variant in &index.variants {
        let set = graph
            .content_of(&variant.id)
            .ok_or_else(|| ArtifactError::UnknownVariant {
                id: variant.id.clone(),
            })?;
        let config = VariantConfig {
            schema: VARIANT_CONFIG_SCHEMA,
            id: variant.id.clone(),
            target: variant.target.clone(),
            platform: variant.platform.clone(),
            frontend: variant.frontend,
            requirements: variant.requirements.clone(),
            manifest: variant.manifest,
            content: set.digests.clone(),
            logical_size: variant.logical_size,
        };
        let config_bytes = serde_json::to_vec(&config)?;
        let config_descriptor = OciDescriptor::new(
            OciMediaType::Other(OCI_CONFIG_MEDIA_TYPE.to_owned()),
            config_bytes.len() as u64,
            sha256_of(&config_bytes),
        );
        let mut layers = Vec::with_capacity(set.digests.len());
        for digest in &set.digests {
            let entry = graph
                .table()
                .entry(digest)
                .ok_or_else(|| ArtifactError::Missing {
                    media_type: MediaType::BLOB.label(),
                    digest: digest.to_hex(),
                })?;
            layers.push(OciDescriptor::new(
                OciMediaType::Other(MediaType::BLOB.as_str().to_owned()),
                entry.size,
                oci_digest(&entry.digest),
            ));
        }
        let manifest = ImageManifestBuilder::default()
            .schema_version(OCI_SCHEMA_VERSION)
            .media_type(OciMediaType::ImageManifest)
            .config(config_descriptor.clone())
            .layers(layers)
            .annotations(HashMap::from([(
                OCI_VARIANT_TITLE.to_owned(),
                variant.id.clone(),
            )]))
            .build()
            .map_err(|_| ArtifactError::Invalid)?;
        let bytes = serde_json::to_vec(&manifest)?;
        let mut descriptor = oci_descriptor(
            &Descriptor::of(MediaType::Index, &bytes),
            OCI_MANIFEST_MEDIA_TYPE,
        );
        descriptor.set_platform(Some(oci_platform(&variant.platform)?));
        descriptor.set_annotations(Some(HashMap::from([(
            OCI_VARIANT_TITLE.to_owned(),
            variant.id.clone(),
        )])));
        descriptors.push(descriptor);
        variants.push(OciVariant {
            id: variant.id.clone(),
            manifest,
            config: config_bytes,
            config_descriptor,
        });
    }
    Ok(OciLayout {
        index: ImageIndexBuilder::default()
            .schema_version(OCI_SCHEMA_VERSION)
            .media_type(OciMediaType::ImageIndex)
            .manifests(descriptors)
            .annotations(HashMap::from([(
                OCI_ARTIFACT.to_owned(),
                index.artifact.id.clone(),
            )]))
            .build()
            .map_err(|_| ArtifactError::Invalid)?,
        variants,
    })
}

fn oci_platform(platform: &Platform) -> Result<OciPlatform, ArtifactError> {
    // OCI names the same machines with its own spellings, so this is a
    // translation table rather than a formatting trick. An architecture the
    // table does not know passes through unchanged rather than being guessed at.
    let architecture = match platform.architecture.as_str() {
        "x86" => Arch::i386,
        "x86_64" => Arch::Amd64,
        "arm" => Arch::ARM,
        "aarch64" => Arch::ARM64,
        other => Arch::Other(other.to_owned()),
    };
    let os = match platform.os.as_str() {
        "windows" => Os::Windows,
        "linux" => Os::Linux,
        "macos" => Os::Darwin,
        "ios" => Os::iOS,
        "android" => Os::Android,
        "freebsd" => Os::FreeBSD,
        "netbsd" => Os::NetBSD,
        "openbsd" => Os::OpenBSD,
        "solaris" => Os::Solaris,
        other => Os::Other(other.to_owned()),
    };
    let builder = PlatformBuilder::default().architecture(architecture).os(os);
    let builder = match platform.variant.clone() {
        Some(variant) => builder.variant(variant),
        None => builder,
    };
    builder.build().map_err(|_| ArtifactError::Invalid)
}

/// Write a composed graph as a local OCI image layout.
///
/// This is the cheapest proof that the model maps: a real `oci-layout`
/// directory an OCI tool can read, built from the same graph the Windows backend
/// writes into an executable.
pub fn export_oci_layout(graph: &ArtifactGraph, destination: &Path) -> Result<(), ArtifactError> {
    let layout = to_oci(graph)?;
    let blobs = destination.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs)?;
    let mut written: Vec<Sha256Digest> = Vec::new();
    let mut write_blob = |bytes: &[u8]| -> Result<Sha256Digest, ArtifactError> {
        let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
        if written.contains(&digest) {
            return Ok(digest);
        }
        let mut file = std::fs::File::create(blobs.join(digest.to_hex()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        written.push(digest);
        Ok(digest)
    };

    for variant in graph.runtimes() {
        write_blob(&variant.bytes)?;
    }
    for manifest in graph.manifests() {
        write_blob(&manifest.bytes)?;
    }
    for variant in &layout.variants {
        write_blob(&variant.config)?;
        write_blob(&serde_json::to_vec(&variant.manifest)?)?;
    }
    if let Ok(segments) = graph.segments() {
        let store = crate::store::SegmentSource::new(graph.table(), &segments);
        for entry in graph.table().entries() {
            write_blob(&store.blob(entry)?)?;
        }
    }
    let index_bytes = serde_json::to_vec(&layout.index)?;
    write_blob(&index_bytes)?;
    std::fs::write(destination.join("index.json"), index_bytes)?;
    std::fs::write(
        destination.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;
    Ok(())
}

/// The blob path OCI uses inside a layout, which is also the canonical
/// immutable content path of a remote store.
pub fn oci_blob_path(digest: &Sha256Digest) -> PathBuf {
    PathBuf::from("blobs").join("sha256").join(digest.to_hex())
}

/// Read a variant's content plan back out of an OCI config blob.
pub fn variant_config(bytes: &[u8]) -> Result<VariantConfig, ArtifactError> {
    let config: VariantConfig = serde_json::from_slice(bytes)?;
    if config.schema != VARIANT_CONFIG_SCHEMA {
        return Err(ArtifactError::Invalid);
    }
    Ok(config)
}

/// The variant an OCI index entry refers to, by the digest of its manifest.
pub fn variant_by_manifest(index: &ImageIndex, digest: &Digest) -> Option<String> {
    index
        .manifests()
        .iter()
        .find(|descriptor| descriptor.digest() == digest)
        .and_then(|descriptor| {
            descriptor
                .annotations()
                .as_ref()
                .and_then(|annotations| annotations.get(OCI_VARIANT_TITLE))
                .cloned()
        })
}

/// Every content digest an OCI variant needs, from its config blob and layers.
///
/// The layers are the store as a registry sees it; the config is the set the
/// variant actually requires. They are equal for a composed graph, and the
/// function reports the union so a partial store is visible rather than assumed
/// complete.
pub fn required_digests(
    manifest: &ImageManifest,
    config_bytes: &[u8],
) -> Result<Vec<Sha256Digest>, ArtifactError> {
    let config = variant_config(config_bytes)?;
    let mut digests = config.content.clone();
    for layer in manifest.layers() {
        if let Some(digest) = zup_digest(layer.digest()) {
            digests.push(digest);
        }
    }
    digests.sort_unstable();
    digests.dedup();
    Ok(digests)
}

/// The OCI platform a zup variant declares, for a registry that selects on it.
pub fn oci_platform_of(variant: &VariantDescriptor) -> Result<OciPlatform, ArtifactError> {
    oci_platform(&variant.platform)
}
