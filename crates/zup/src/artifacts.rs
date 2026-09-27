//! Distribution artifacts on the build side.
//!
//! A target profile becomes a **variant**: one resolved native target with its
//! content, its runtime template, and its requirements. Variants are built
//! locally, in parallel, and are the unit that is cached.
//!
//! An **artifact** is a file a user downloads. It may contain one variant or
//! several, and it is composed after every variant it needs exists, because an
//! artifact may consume outputs from target builds that happened on different
//! machines. That is the local/global split: variants are local, artifacts are
//! global.
//!
//! Nothing here knows about a container format. `zup-windows` turns a composed
//! graph into a PE, and a future backend would turn the same graph into a package
//! bundle or a fat binary.

use std::path::{Path, PathBuf};

use zup_artifact::{
    ArtifactComposer, ArtifactError, ArtifactGraph, ArtifactKind, ArtifactMode, ArtifactPin,
    ArtifactRequest, DistributionVariant, LauncherStrategy, ReleaseManifest,
};
use zup_core::TargetProfileId;

/// The dispatcher template file names, beside the `zup` executable.
pub const DISPATCHER_GUI: &str = "zup-dispatch.exe";
pub const DISPATCHER_CONSOLE: &str = "zup-dispatch-console.exe";

/// One artifact a project declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactProfile {
    pub kind: ArtifactKind,
    pub mode: ArtifactMode,
    /// The target profiles this artifact includes, in declaration order.
    pub targets: Vec<TargetProfileId>,
    /// The release channel this artifact follows, when it follows one.
    ///
    /// An artifact without a channel is labelled with an exact version and always
    /// installs it. An artifact with one resolves the channel's current release,
    /// which is a different promise and a different file name.
    pub channel: Option<String>,
    /// The output file name, when the project names one.
    pub output: Option<String>,
}

impl ArtifactProfile {
    /// The launcher subsystem this artifact's variants must agree on.
    pub fn subsystem(&self, variants: &[&DistributionVariant]) -> zup_artifact::LauncherSubsystem {
        variants
            .first()
            .map_or(zup_artifact::LauncherSubsystem::Console, |variant| {
                variant.subsystem()
            })
    }

    /// The filename this artifact's build writes.
    pub fn file_name(&self, app_name: &str, version: &semver::Version) -> String {
        if let Some(output) = &self.output {
            return output.clone();
        }
        let label = match &self.channel {
            Some(channel) => format!("-{channel}"),
            None => String::new(),
        };
        let _ = version;
        let kind = match self.kind {
            ArtifactKind::Universal => "Windows",
            ArtifactKind::Single => "Setup",
        };
        format!("{app_name}-{kind}{label}-Setup.exe")
    }

    /// The composer request for this artifact.
    pub fn request(
        &self,
        id: &str,
        output: String,
        application: &zup_core::App,
    ) -> ArtifactRequest {
        let pin = match &self.channel {
            Some(channel) => ArtifactPin::Channel {
                channel: channel.clone(),
            },
            None => ArtifactPin::Pinned {
                version: application.version.clone(),
            },
        };
        ArtifactRequest {
            id: id.to_owned(),
            kind: self.kind,
            mode: self.mode,
            pin,
            launcher: LauncherStrategy::EmbeddedDispatcher,
            output,
        }
    }

    /// The dispatcher template this artifact's launcher experience needs.
    pub fn dispatcher_name(&self, variants: &[&DistributionVariant]) -> &'static str {
        match self.subsystem(variants) {
            zup_artifact::LauncherSubsystem::Gui => DISPATCHER_GUI,
            zup_artifact::LauncherSubsystem::Console => DISPATCHER_CONSOLE,
        }
    }
}

/// A profile selection that composition refuses, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionRefusal {
    pub dimension: zup_artifact::CompatibilityDimension,
    pub message: String,
}

/// Check whether the selected variants can form one artifact, and say what
/// composing them would save.
///
/// This is the read-only half of composition, so `zup check` and `zup doctor`
/// can show the value of a universal artifact before anyone builds one.
pub fn composition_report(
    variants: &[&DistributionVariant],
) -> Result<CompositionSavings, CompositionRefusal> {
    if variants.len() < 2 {
        return Ok(CompositionSavings {
            standalone_size: variants.first().map_or(0, |variant| variant.logical_size()),
            shared_size: 0,
            exclusive_size: variants.first().map_or(0, |variant| variant.logical_size()),
            unique_blob_count: variants
                .first()
                .map_or(0, |variant| variant.content_digests().len() as u64),
        });
    }
    let mut counts: std::collections::BTreeMap<zup_core::Sha256Digest, (usize, u64)> =
        std::collections::BTreeMap::new();
    for variant in variants {
        for digest in variant.content_digests() {
            let (_, size) = entry_size(variants, digest);
            let slot = counts.entry(digest).or_insert((0, size));
            slot.0 += 1;
        }
    }
    let mut shared_size = 0u64;
    let mut exclusive_size = 0u64;
    for (references, size) in counts.values() {
        if *references > 1 {
            shared_size = shared_size.saturating_add(*size);
        } else {
            exclusive_size = exclusive_size.saturating_add(*size);
        }
    }
    Ok(CompositionSavings {
        standalone_size: variants.iter().fold(0u64, |sum, variant| {
            sum.saturating_add(variant.logical_size())
        }),
        shared_size,
        exclusive_size,
        unique_blob_count: counts.len() as u64,
    })
}

/// What composing a selection of variants would save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionSavings {
    pub standalone_size: u64,
    pub shared_size: u64,
    pub exclusive_size: u64,
    pub unique_blob_count: u64,
}

fn entry_size(variants: &[&DistributionVariant], digest: zup_core::Sha256Digest) -> (usize, u64) {
    for variant in variants {
        for entry in &variant.plan().entries {
            if entry.blob == digest {
                return (1, entry.size);
            }
        }
        for artifact in &variant.plan().prerequisite_artifacts {
            if artifact.blob == digest {
                return (1, artifact.size);
            }
        }
        for artifact in &variant.plan().plugins {
            if artifact.blob == digest {
                return (1, artifact.aot_size);
            }
        }
    }
    (0, 0)
}

/// Compose one artifact from the variants it names.
pub fn compose(
    request: ArtifactRequest,
    variants: &[&DistributionVariant],
) -> Result<ArtifactGraph, ArtifactError> {
    ArtifactComposer::new(request, variants)?.compose(variants)
}

/// Find a dispatcher template beside an executable, or beside the running one.
pub fn discover_dispatcher(name: &str) -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let candidate = path.join(name);
    candidate.is_file().then_some(candidate)
}

/// The launcher subsystem a frontend's installer experience needs.
pub fn subsystem_of(frontend: zup_core::Frontend) -> zup_artifact::LauncherSubsystem {
    zup_artifact::frontend_subsystem(frontend)
}

/// The artifact output paths a caller asked for, or the derived ones.
pub fn resolve_outputs(
    supplied: &[PathBuf],
    jobs: &[(String, String)],
    manifest_path: &Path,
) -> miette::Result<Vec<PathBuf>> {
    let parent = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    if supplied.is_empty() {
        return Ok(jobs
            .iter()
            .map(|(_, file_name)| parent.join(file_name))
            .collect());
    }
    if supplied.len() != jobs.len() {
        return Err(miette::miette!(
            "selected {} artifacts ({}) but received {} outputs; provide one --output per artifact, in that order",
            jobs.len(),
            names(jobs),
            supplied.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(supplied.len());
    for path in supplied {
        if !seen.insert(path.clone()) {
            return Err(miette::miette!(
                "output `{}` is used by more than one artifact",
                path.display()
            ));
        }
        out.push(path.clone());
    }
    Ok(out)
}

fn names(jobs: &[(String, String)]) -> String {
    jobs.iter()
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Start a release description for a project.
pub fn release_manifest(application: &zup_core::App) -> ReleaseManifest {
    ReleaseManifest::new(application)
}
