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

fn sha256_of(bytes: &[u8]) -> oci_spec::image::Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
        .to_hex()
        .parse()
        .expect("a lowercase hex SHA-256 digest parses as an OCI digest")
}

use crate::compose::ArtifactGraph;
use crate::format::ArtifactError;
use crate::format::Descriptor;
use crate::format::MediaType;
use crate::platform::Platform;
use crate::variant::{VariantDescriptor, VariantRequirements};

pub const OCI_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

pub const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

pub const OCI_CONFIG_MEDIA_TYPE: &str = "application/vnd.zup.artifact.variant-config.v1+json";

pub const OCI_VARIANT_TITLE: &str = "org.opencontainers.image.title";

pub const OCI_ARTIFACT: &str = "io.zup.artifact.id";

pub const OCI_SCHEMA_VERSION: u32 = 2;

pub const VARIANT_CONFIG_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantConfig {
    pub schema: u32,
    pub id: String,
    pub target: zup_core::TargetTriple,
    pub platform: Platform,
    pub frontend: zup_core::Frontend,
    pub requirements: VariantRequirements,
    pub manifest: Descriptor,
    pub content: Vec<Sha256Digest>,
    pub logical_size: u64,
}

pub fn oci_digest(digest: &Sha256Digest) -> Digest {
    Digest::from(
        digest
            .to_hex()
            .parse::<oci_spec::image::Sha256Digest>()
            .expect("a lowercase hex SHA-256 digest parses as an OCI digest"),
    )
}

pub fn zup_digest(digest: &Digest) -> Option<Sha256Digest> {
    match digest.algorithm() {
        oci_spec::image::DigestAlgorithm::Sha256 => digest.digest().parse().ok(),
        _ => None,
    }
}

pub fn oci_descriptor(descriptor: &Descriptor, media_type: &str) -> OciDescriptor {
    OciDescriptor::new(
        OciMediaType::Other(media_type.to_owned()),
        descriptor.size,
        oci_digest(&descriptor.digest),
    )
}

#[derive(Debug, Clone)]
pub struct OciVariant {
    pub id: String,
    pub manifest: ImageManifest,
    pub config: Vec<u8>,
    pub config_descriptor: OciDescriptor,
}

#[derive(Debug, Clone)]
pub struct OciLayout {
    pub index: ImageIndex,
    pub variants: Vec<OciVariant>,
}

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

pub fn oci_blob_path(digest: &Sha256Digest) -> PathBuf {
    PathBuf::from("blobs").join("sha256").join(digest.to_hex())
}

pub fn variant_config(bytes: &[u8]) -> Result<VariantConfig, ArtifactError> {
    let config: VariantConfig = serde_json::from_slice(bytes)?;
    if config.schema != VARIANT_CONFIG_SCHEMA {
        return Err(ArtifactError::Invalid);
    }
    Ok(config)
}

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

pub fn oci_platform_of(variant: &VariantDescriptor) -> Result<OciPlatform, ArtifactError> {
    oci_platform(&variant.platform)
}
