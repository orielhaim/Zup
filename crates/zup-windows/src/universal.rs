// Stays in zup-windows: variant selection and staging orchestrate host detection, bundle identity, and durable I/O; only the resource primitives live in zup-pe.
use std::path::Path;

use thiserror::Error;
use zup_artifact::{
    ArtifactError, ArtifactGraph, ArtifactView, ContentSource, Descriptor, MediaType, MetadataSet,
    SegmentReader, select_from_index,
};
use zup_binary::{BinaryArchitecture, Executable, ProgramKind};
use zup_core::TargetTriple;
use zup_pe::{RESOURCE_ID_BLOB_START, RESOURCE_ID_INDEX, ResourceDocument};
use zup_transaction::{MAINTENANCE_INDEX_NAME, MAINTENANCE_PACKAGE_NAME};

use crate::host;
#[derive(Debug, Error)]
pub enum UniversalError {
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    Inspect(#[from] zup_binary::InspectError),
    #[error(transparent)]
    Portable(#[from] zup_pe::PeError),
    #[error(transparent)]
    Package(#[from] zup_bundle::PackageError),
    #[error(transparent)]
    Bundle(#[from] crate::BundleError),
    #[error(transparent)]
    Durable(#[from] crate::durable::DurableError),
    #[error("image resources: {0}")]
    Resources(#[from] crate::pe_resources::ResourceError),
    #[error("universal artifact I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the dispatcher template is a {found} program but the artifact is a {expected} launcher experience"
    )]
    DispatcherTemplate {
        expected: zup_artifact::LauncherSubsystem,

        found: zup_artifact::LauncherSubsystem,
    },
    #[error("dispatcher template is already signed; compose before signing")]
    DispatcherSigned,
    #[error("dispatcher template records no window/terminal subsystem; it is not a launcher")]
    DispatcherSubsystem,
    #[error(
        "the dispatcher is a {found} program, but the artifact includes a {narrowest} variant; a machine that can run that variant must be able to start the dispatcher first"
    )]
    DispatcherTooWide {
        found: BinaryArchitecture,
        narrowest: TargetTriple,
    },
    #[error("artifact `{id}` has no variant this host can run")]
    UnsupportedHost { id: String },
    #[error("artifact `{id}` does not carry the named variant")]
    UnknownVariant { id: String },
    #[error(
        "a thin artifact embeds no runtime bytes; its runtime is fetched and verified by digest"
    )]
    ThinRuntime,
}

impl UniversalError {
    pub fn is_unsupported_host(&self) -> bool {
        match self {
            Self::Artifact(error) => error.is_unsupported_host(),
            Self::UnsupportedHost { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniversalLayout {
    pub variants: usize,

    pub segments: usize,

    pub first_segment: usize,
}

impl UniversalLayout {
    pub const fn new(variants: usize, segments: usize) -> Self {
        let first_segment = RESOURCE_ID_BLOB_START + 1 + variants * 2;
        assert!(
            first_segment + segments <= zup_pe::RESOURCE_ID_PRESET,
            "a universal artifact's content runs into the identifier a self-contained \
             installer's preset occupies"
        );
        Self {
            variants,
            segments,
            first_segment,
        }
    }

    pub const fn table(&self) -> usize {
        RESOURCE_ID_INDEX + 1
    }

    pub const fn manifest(&self, index: usize) -> usize {
        RESOURCE_ID_BLOB_START + 1 + index
    }

    pub const fn runtime(&self, index: usize) -> usize {
        RESOURCE_ID_BLOB_START + 1 + self.variants + index
    }

    pub const fn segment(&self, segment: usize) -> usize {
        self.first_segment + segment
    }
}

pub fn compose_universal_executable(
    dispatcher: &Path,
    output: &Path,
    graph: &ArtifactGraph,
) -> Result<UniversalLayout, UniversalError> {
    let expected = graph.index().artifact.subsystem;
    let template = Executable::read(dispatcher)?;
    let found = match template.program() {
        Some(ProgramKind::Windowed) => zup_artifact::LauncherSubsystem::Gui,
        Some(ProgramKind::Console) => zup_artifact::LauncherSubsystem::Console,
        None => return Err(UniversalError::DispatcherSubsystem),
    };
    if found != expected {
        return Err(UniversalError::DispatcherTemplate { expected, found });
    }
    if zup_pe::is_signed(dispatcher)? {
        return Err(UniversalError::DispatcherSigned);
    }
    if let Some((width, narrowest)) = narrowest_variant(graph)
        && let Some(machine) = template.architecture()
        && machine_width(machine) > width
    {
        return Err(UniversalError::DispatcherTooWide {
            found: machine,
            narrowest,
        });
    }
    let segments = graph.segments()?;
    let layout = UniversalLayout::new(graph.index().variants.len(), graph.segment_sizes().len());
    let mut documents = vec![
        ResourceDocument {
            id: RESOURCE_ID_INDEX,
            bytes: graph.index_bytes()?,
        },
        ResourceDocument {
            id: layout.table(),
            bytes: graph.table_bytes()?,
        },
    ];
    for (index, (_, bytes)) in graph.manifest_bytes()?.into_iter().enumerate() {
        documents.push(ResourceDocument {
            id: layout.manifest(index),
            bytes,
        });
    }
    for (index, (_, bytes)) in graph.runtime_bytes()?.into_iter().enumerate() {
        documents.push(ResourceDocument {
            id: layout.runtime(index),
            bytes,
        });
    }
    for (segment, size) in graph.segment_sizes().iter().enumerate() {
        let range = segments
            .read_range(segment as u16, 0, *size)
            .map_err(UniversalError::Artifact)?;
        documents.push(ResourceDocument {
            id: layout.segment(segment),
            bytes: range,
        });
    }
    crate::pe_resources::write_resources(dispatcher, output, &documents)?;
    Ok(layout)
}

fn narrowest_variant(graph: &ArtifactGraph) -> Option<(u8, TargetTriple)> {
    graph
        .index()
        .variants
        .iter()
        .filter_map(|variant| {
            BinaryArchitecture::of_target(&variant.target)
                .map(|machine| (machine_width(machine), variant.target.clone()))
        })
        .min_by_key(|(width, _)| *width)
}

const fn machine_width(machine: BinaryArchitecture) -> u8 {
    match machine {
        BinaryArchitecture::X86_32 | BinaryArchitecture::Arm32 => 0,
        BinaryArchitecture::X86_64 => 1,
        BinaryArchitecture::Arm64 => 2,
    }
}

pub struct UniversalArtifact {
    executable: std::path::PathBuf,
    view: ArtifactView<PeSegments>,
}

impl UniversalArtifact {
    pub fn open(executable: impl AsRef<Path>) -> Result<Self, UniversalError> {
        let executable = executable.as_ref().to_path_buf();
        let index_bytes = crate::pe_resources::read_resource(&executable, RESOURCE_ID_INDEX)?;
        let index = zup_artifact::ArtifactIndex::parse(&index_bytes)?;
        let layout = UniversalLayout::new(index.variants.len(), 0);
        let table_bytes = crate::pe_resources::read_resource(&executable, layout.table())?;

        let carries_runtimes = index.artifact.mode.carries_content();
        let mut metadata = MetadataSet::new();
        for (position, variant) in index.variants.iter().enumerate() {
            metadata.insert(
                &variant.manifest,
                read_metadata(&executable, layout.manifest(position))?,
            )?;
            if carries_runtimes && let Some(runtime) = variant.runtime {
                metadata.insert(
                    &runtime,
                    read_metadata(&executable, layout.runtime(position))?,
                )?;
            }
        }
        let view = ArtifactView::open(
            &index_bytes,
            &table_bytes,
            metadata,
            PeSegments {
                executable: executable.clone(),
                first: layout.first_segment,

                count: u16::MAX,
            },
        )?;
        Ok(Self { executable, view })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn view(&self) -> &ArtifactView<PeSegments> {
        &self.view
    }

    pub fn index(&self) -> &zup_artifact::ArtifactIndex {
        self.view.index()
    }

    pub fn select(&self) -> Result<Selection, UniversalError> {
        let host = host::host_execution();
        let selected = select_from_index(&host, self.index())?;
        let compatibility = selected.compatibility;
        let id = selected.candidate.id.to_owned();
        Ok(Selection {
            id,
            compatibility,
            emulated: compatibility == zup_artifact::Compatibility::Emulated,
        })
    }

    pub fn embedded_runtime(&self, variant: &str) -> Result<Vec<u8>, UniversalError> {
        if !self.index().artifact.mode.carries_content() {
            return Err(UniversalError::ThinRuntime);
        }
        let position = self
            .index()
            .variant_ids()
            .iter()
            .position(|id| *id == variant)
            .ok_or_else(|| UniversalError::UnknownVariant {
                id: variant.to_owned(),
            })?;
        let layout = UniversalLayout::new(self.index().variants.len(), 0);
        Ok(crate::pe_resources::read_resource(
            &self.executable,
            layout.runtime(position),
        )?)
    }
}

fn read_metadata(executable: &Path, id: usize) -> Result<Vec<u8>, UniversalError> {
    Ok(crate::pe_resources::read_resource(executable, id)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub id: String,
    pub compatibility: zup_artifact::Compatibility,

    pub emulated: bool,
}

pub struct PeSegments {
    executable: std::path::PathBuf,
    first: usize,
    count: u16,
}

impl SegmentReader for PeSegments {
    fn read_range(&self, segment: u16, offset: u64, len: u64) -> Result<Vec<u8>, ArtifactError> {
        let id = self.first + usize::from(segment);
        let bytes = crate::pe_resources::read_resource(&self.executable, id).map_err(|_| {
            ArtifactError::Missing {
                media_type: MediaType::BLOB.label(),
                digest: format!("segment {segment}"),
            }
        })?;
        let start = usize::try_from(offset).map_err(|_| ArtifactError::Invalid)?;
        let end = start
            .checked_add(usize::try_from(len).map_err(|_| ArtifactError::Invalid)?)
            .ok_or(ArtifactError::Invalid)?;
        bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or(ArtifactError::Invalid)
    }

    fn segment_count(&self) -> u16 {
        self.count
    }
}

pub fn read_variant_manifest(
    artifact: &UniversalArtifact,
    id: &str,
) -> Result<zup_artifact::VariantManifest, UniversalError> {
    Ok(artifact.view().variant_manifest(id)?)
}

pub fn verify_selected_variant(
    artifact: &UniversalArtifact,
    id: &str,
) -> Result<zup_artifact::VariantManifest, UniversalError> {
    Ok(artifact.view().verify_variant(id)?)
}

pub struct StagedVariant {
    pub runtime: std::path::PathBuf,

    pub package: std::path::PathBuf,

    pub index: std::path::PathBuf,
}

pub fn stage_variant(
    artifact: &UniversalArtifact,
    id: &str,
    directory: &Path,
) -> Result<StagedVariant, UniversalError> {
    let manifest = verify_selected_variant(artifact, id)?;
    let variant = artifact
        .index()
        .variant(id)
        .ok_or_else(|| UniversalError::UnsupportedHost {
            id: artifact.index().artifact.id.clone(),
        })?;
    let runtime_descriptor = variant
        .runtime
        .ok_or_else(|| UniversalError::UnsupportedHost {
            id: artifact.index().artifact.id.clone(),
        })?;
    let runtime_bytes = artifact.view().read(&runtime_descriptor)?;
    std::fs::create_dir_all(directory)?;

    let runtime = directory.join(format!(
        "{}{}",
        zup_transaction::MAINTENANCE_RUNTIME_DIRECTORY,
        variant.target.executable_suffix()
    ));
    write_durable(&runtime, &runtime_bytes)?;
    let package = directory.join(MAINTENANCE_PACKAGE_NAME);
    write_variant_package(artifact, &manifest, &package)?;
    let index = directory.join(MAINTENANCE_INDEX_NAME);
    write_durable(&index, &artifact.index().encode()?)?;
    Ok(StagedVariant {
        runtime,
        package,
        index,
    })
}

fn write_variant_package(
    artifact: &UniversalArtifact,
    manifest: &zup_artifact::VariantManifest,
    destination: &Path,
) -> Result<(), UniversalError> {
    let store = artifact.view().store();
    let mut compressed = std::collections::BTreeMap::new();
    for digest in manifest.content_digests() {
        let entry = artifact
            .view()
            .table()
            .entry(&digest)
            .ok_or(ArtifactError::Missing {
                media_type: MediaType::BLOB.label(),
                digest: digest.to_hex(),
            })?;

        let content = store.blob(entry)?;
        let encoded = zstd::stream::encode_all(std::io::Cursor::new(content.as_slice()), 9)?;
        compressed.insert(digest, encoded);
    }
    let temporary = destination.with_extension("zup-partial");
    if temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    zup_bundle::BundleWriter::write_plan(&manifest.plan, &compressed, &temporary)?;
    std::fs::rename(&temporary, destination)?;
    Ok(())
}

fn write_durable(path: &Path, bytes: &[u8]) -> Result<(), UniversalError> {
    crate::durable::write_durable(path, bytes)?;
    Ok(())
}

pub fn staged_descriptor(media_type: MediaType, bytes: &[u8]) -> Descriptor {
    Descriptor::of(media_type, bytes)
}
