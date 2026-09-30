//! The Windows universal artifact backend.
//!
//! One file, several native runtimes, one shared content store. The file is a
//! small dispatcher with the composed graph in its resources:
//!
//! ```text
//! Acme-Windows-Setup.exe
//!     dispatcher                  a launcher, not an installer
//!     resource 1                  the artifact index
//!     resource 2                  the content store table
//!     resources 3..               one variant manifest per variant
//!     resources ..                one native runtime per variant
//!     resources ..                the content store, one region per segment
//! ```
//!
//! Everything lives inside Authenticode-hashed image sections, because the
//! resources are written before the finished image is signed. There is no
//! trailing overlay, so a signature covers the whole artifact.
//!
//! The dispatcher deliberately has no lifecycle authority. It selects a variant,
//! stages that variant's native runtime and content, starts it, and exits. Every
//! privileged operation happens inside the selected native runtime, in its own
//! architecture, which is the reason the dispatcher can be small enough to run
//! under an emulation layer on a machine whose native variant is something else.

use std::path::Path;

use thiserror::Error;
use zup_artifact::{
    ArtifactError, ArtifactGraph, ArtifactView, ContentSource, Descriptor, MediaType, MetadataSet,
    SegmentReader, select_from_index,
};
use zup_core::TargetTriple;
use zup_pe::{Machine, RESOURCE_ID_BLOB_START, RESOURCE_ID_INDEX, ResourceDocument};

use crate::host;

/// Failures produced by the Windows universal backend.
#[derive(Debug, Error)]
pub enum UniversalError {
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
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
        /// The launcher experience the artifact's variants agreed on.
        expected: zup_artifact::LauncherSubsystem,
        /// The experience the template actually presents.
        found: zup_artifact::LauncherSubsystem,
    },
    #[error("dispatcher template is already signed; compose before signing")]
    DispatcherSigned,
    #[error(
        "the dispatcher is a {found} program, but the artifact includes a {narrowest} variant; a machine that can run that variant must be able to start the dispatcher first"
    )]
    DispatcherTooWide {
        found: Machine,
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
    /// Whether this failure is a host no variant can serve, which is a supported
    /// refusal the user is told about rather than a defect.
    pub fn is_unsupported_host(&self) -> bool {
        match self {
            Self::Artifact(error) => error.is_unsupported_host(),
            Self::UnsupportedHost { .. } => true,
            _ => false,
        }
    }
}

/// The resource identifiers a universal artifact uses.
///
/// The layout is fixed and total, so a reader knows exactly which identifier
/// holds what without consulting the index first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniversalLayout {
    /// How many variants the artifact carries.
    pub variants: usize,
    /// How many content segments the artifact carries.
    pub segments: usize,
    /// One past the last identifier the metadata documents use.
    pub first_segment: usize,
}

impl UniversalLayout {
    /// The layout of `variants` variants and `segments` content segments.
    pub const fn new(variants: usize, segments: usize) -> Self {
        // The index and the table take the first two identifiers; then a
        // manifest and a runtime per variant; then the content segments.
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

    /// The identifier the blob table occupies.
    pub const fn table(&self) -> usize {
        RESOURCE_ID_INDEX + 1
    }

    /// The identifier of variant `index`'s manifest.
    pub const fn manifest(&self, index: usize) -> usize {
        RESOURCE_ID_BLOB_START + 1 + index
    }

    /// The identifier of variant `index`'s native runtime.
    pub const fn runtime(&self, index: usize) -> usize {
        RESOURCE_ID_BLOB_START + 1 + self.variants + index
    }

    /// The identifier of content `segment`.
    pub const fn segment(&self, segment: usize) -> usize {
        self.first_segment + segment
    }
}

/// Compose a universal artifact into a copy of a dispatcher template.
///
/// The template must be unsigned, must match the launcher subsystem the
/// artifact's variants agreed on, and must be no wider than the narrowest
/// machine any included variant targets. That last rule is what makes a
/// universal artifact universal: the dispatcher is the first thing the machine
/// has to be able to start, so a host that can run the narrowest variant must be
/// able to run the dispatcher too. A 32-bit x86 template satisfies that for
/// every Windows host, which is why that is what `zup-dispatch` builds.
pub fn compose_universal_executable(
    dispatcher: &Path,
    output: &Path,
    graph: &ArtifactGraph,
) -> Result<UniversalLayout, UniversalError> {
    let expected = graph.index().artifact.subsystem;
    let header = zup_pe::read_pe_header(dispatcher)?;
    let found = match header.subsystem {
        zup_pe::Subsystem::Gui => zup_artifact::LauncherSubsystem::Gui,
        zup_pe::Subsystem::Console => zup_artifact::LauncherSubsystem::Console,
        zup_pe::Subsystem::Other(_) => {
            return Err(crate::BundleError::Invalid.into());
        }
    };
    if found != expected {
        return Err(UniversalError::DispatcherTemplate { expected, found });
    }
    if zup_pe::is_signed(dispatcher)? {
        return Err(UniversalError::DispatcherSigned);
    }
    if let Some(narrowest) = narrowest_variant(graph)
        && machine_width(header.machine) > machine_width(machine_of(&narrowest))
    {
        return Err(UniversalError::DispatcherTooWide {
            found: header.machine,
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

/// The narrowest machine any included variant targets.
///
/// Windows runs 32-bit x86 everywhere, and runs 64-bit only where the operating
/// system is 64-bit, so the narrowest variant is the one that decides how wide a
/// dispatcher may be.
fn narrowest_variant(graph: &ArtifactGraph) -> Option<TargetTriple> {
    graph
        .index()
        .variants
        .iter()
        .map(|variant| variant.target.clone())
        .min_by_key(|target| machine_width(machine_of(target)))
}

/// The machine a Windows target triple names.
fn machine_of(target: &TargetTriple) -> Machine {
    use zup_core::TargetArchitecture;
    match target.architecture() {
        TargetArchitecture::X86_32(_) => Machine::I386,
        TargetArchitecture::X86_64 => Machine::Amd64,
        TargetArchitecture::Aarch64(_) => Machine::Arm64,
        _ => Machine::Other(0),
    }
}

/// How wide a machine is, in the one order that matters: a machine runs
/// everything narrower than itself.
const fn machine_width(machine: Machine) -> u8 {
    match machine {
        Machine::I386 => 0,
        Machine::Amd64 => 1,
        Machine::Arm64 => 2,
        Machine::Other(_) => u8::MAX,
    }
}

/// A universal artifact opened for reading.
///
/// This is the dispatcher and inspector's view of one file. It reads the same
/// index the native runtime would read, so a format rule exists once.
pub struct UniversalArtifact {
    executable: std::path::PathBuf,
    view: ArtifactView<PeSegments>,
}

impl UniversalArtifact {
    /// Open a universal artifact, validating its index, table, and layout.
    pub fn open(executable: impl AsRef<Path>) -> Result<Self, UniversalError> {
        let executable = executable.as_ref().to_path_buf();
        let index_bytes = crate::pe_resources::read_resource(&executable, RESOURCE_ID_INDEX)?;
        let index = zup_artifact::ArtifactIndex::parse(&index_bytes)?;
        let layout = UniversalLayout::new(index.variants.len(), 0);
        let table_bytes = crate::pe_resources::read_resource(&executable, layout.table())?;
        // A thin artifact names each variant's runtime so a client knows what the
        // graph will hand it, but it does not carry one - the runtime is the
        // thing the artifact exists to fetch, and embedding it would make the
        // installer the application. An offline artifact carries it, because it
        // has to be able to execute what it holds.
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
                // A resource-addressed store is bounded by the table, not by a
                // count this reader could know in advance.
                count: u16::MAX,
            },
        )?;
        Ok(Self { executable, view })
    }

    /// The path this artifact was opened from.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The parsed artifact.
    pub fn view(&self) -> &ArtifactView<PeSegments> {
        &self.view
    }

    /// The index, for reporting and selection.
    pub fn index(&self) -> &zup_artifact::ArtifactIndex {
        self.view.index()
    }

    /// Select the variant this host should run.
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

    /// The native runtime bytes this artifact embeds for `variant`.
    ///
    /// This is the file that becomes `maintenance.exe` on a user's machine, the
    /// elevated worker, and the uninstall runner. A release pipeline needs to read
    /// it for one reason: to prove that the bytes it embedded are the bytes it
    /// signed, since the outer artifact's Authenticode signature covers these as
    /// resource data and does not travel with the extracted file.
    ///
    /// A thin artifact has no embedded runtime by construction, and says so
    /// rather than returning an empty vector a caller might mistake for one.
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

/// The variant a host selected, with how it will be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub id: String,
    pub compatibility: zup_artifact::Compatibility,
    /// Whether the selected variant will run under a compatibility or
    /// emulation layer rather than natively.
    pub emulated: bool,
}

/// Content segments read out of an image's resources.
///
/// A reader over a container rather than a directory, which is what lets the
/// dispatcher, the runtime, and the inspector all read the same artifact through
/// the same table.
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

/// Read a universal artifact's variant manifest through the shared parser.
pub fn read_variant_manifest(
    artifact: &UniversalArtifact,
    id: &str,
) -> Result<zup_artifact::VariantManifest, UniversalError> {
    Ok(artifact.view().variant_manifest(id)?)
}

/// Verify one variant completely before anything is staged from it: the manifest
/// matches its descriptor, and every blob the variant needs is present.
pub fn verify_selected_variant(
    artifact: &UniversalArtifact,
    id: &str,
) -> Result<zup_artifact::VariantManifest, UniversalError> {
    Ok(artifact.view().verify_variant(id)?)
}

/// Write the selected variant's native runtime and content into `directory`.
///
/// This is composition, not installation: it creates two files and returns the
/// digests it proved, so the caller can record them in whatever transaction it is
/// running. It writes nothing else, opens nothing privileged, and reads no
/// registry.
pub struct StagedVariant {
    /// The native runtime image, ready to execute.
    pub runtime: std::path::PathBuf,
    /// The selected variant's package: its manifest and exactly the blobs it
    /// needs, in the portable package format the native runtime already reads.
    pub package: std::path::PathBuf,
    /// The artifact index, so a later run knows what it is looking at.
    pub index: std::path::PathBuf,
}

/// Materialize one variant into `directory`.
///
/// The package is written by streaming the variant's blobs out of the shared
/// store, so a machine never persists a blob belonging to another architecture.
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
    let runtime = directory.join(crate::MAINTENANCE_EXECUTABLE_NAME);
    write_durable(&runtime, &runtime_bytes)?;
    let package = directory.join(crate::MAINTENANCE_PACKAGE_NAME);
    write_variant_package(artifact, &manifest, &package)?;
    let index = directory.join(crate::MAINTENANCE_INDEX_NAME);
    write_durable(&index, &artifact.index().encode()?)?;
    Ok(StagedVariant {
        runtime,
        package,
        index,
    })
}

/// Write the portable package a native runtime reads, containing exactly the
/// selected variant's content.
///
/// The blobs are taken from the shared store as compressed bytes, so nothing is
/// decompressed and recompressed, and nothing belonging to another architecture
/// is written.
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
        // Read through the verified store: the bytes are decompressed, hashed,
        // and only then recompressed into the package's own framing, so a
        // corrupt store cannot be laundered into a package that opens.
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

/// A descriptor for a staged file, for a caller that records what it wrote.
pub fn staged_descriptor(media_type: MediaType, bytes: &[u8]) -> Descriptor {
    Descriptor::of(media_type, bytes)
}
