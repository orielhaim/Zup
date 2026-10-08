use serde::{Deserialize, Serialize};
use zup_artifact::ArtifactError;
use zup_automation::{Application, AutomationResult, Details, LogLevel, Target};
#[cfg(windows)]
use zup_windows::UniversalArtifact;

use crate::failure::Reporter;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub artifact: String,
    pub application: String,
    pub application_version: String,
    pub kind: String,
    pub mode: String,
    pub pin: String,
    pub subsystem: String,
    pub variants: Vec<InspectedVariant>,
    pub content: InspectedContent,
    pub trust: InspectedTrust,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedVariant {
    pub id: String,
    pub target: String,
    pub frontend: String,
    pub logical_size: u64,
    pub file_count: u64,
    pub prerequisite_count: u64,
    pub plugin_count: u64,
    pub native_execution: bool,
    /// statement. A report that printed one and never compared them would be
    pub target_matches_binary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedContent {
    pub logical_size: u64,
    pub stored_size: u64,
    pub content_size: u64,
    pub shared_size: u64,
    pub exclusive_size: u64,
    pub unique_blob_count: u64,
    pub file_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedTrust {
    pub authenticode: String,
    pub index: String,
    pub content_digests: String,
    pub variants: String,
}

#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[cfg(windows)]
    #[error(transparent)]
    Universal(#[from] zup_windows::UniversalError),
    #[error(transparent)]
    Portable(#[from] zup_pe::PeError),
    #[error(transparent)]
    Package(#[from] zup_bundle::PackageError),
    #[error(transparent)]
    Carrier(#[from] zup_linux::CarrierError),
    #[error("artifact I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Every content digest is verified, so a report never describes content the
pub fn run(args: crate::cli::ArtifactInspectCommand) -> miette::Result<AutomationResult> {
    let reporter = Reporter::new(args.format);
    let report = inspect(&args.artifact).map_err(|error| {
        crate::failure::error(
            "zup.artifact.unreadable",
            format!("`{}`: {error}", args.artifact.display()),
        )
    })?;
    reporter.log(LogLevel::Info, report.human());
    Ok(
        AutomationResult::new(zup_automation::OPERATION_ARTIFACT_INSPECT)
            .with_application(Application {
                id: args.artifact.display().to_string(),
                name: report.application.clone(),
                version: report.application_version.clone(),
            })
            .with_targets(
                report
                    .variants
                    .iter()
                    .map(|variant| Target::new(variant.id.clone(), variant.target.clone()))
                    .collect(),
            )
            .with_details(Details::ArtifactInspect(crate::automation::inspection(
                &report,
            )))
            .with_summary(format!(
                "{} {} · {} variant(s) · {} on disk",
                report.application,
                report.application_version,
                report.variants.len(),
                zup_presentation::format_bytes(report.content.file_size)
            )),
    )
}

pub fn inspect(path: &std::path::Path) -> Result<Inspection, InspectError> {
    #[cfg(windows)]
    {
        Ok(match UniversalArtifact::open(path) {
            Ok(artifact) => inspect_composed(path, &artifact)?,
            Err(first) => match inspect_carrier(path) {
                Ok(inspection) => inspection,
                Err(_) => return Err(InspectError::Universal(first)),
            },
        })
    }
    #[cfg(not(windows))]
    {
        inspect_carrier(path)
    }
}

/// package parse, and image/target pairing - so a report never describes an
fn inspect_carrier(path: &std::path::Path) -> Result<Inspection, InspectError> {
    let carrier = zup_linux::Carrier::open(path)?;
    let plan = carrier
        .package()
        .build_plan()?
        .targets
        .into_iter()
        .next()
        .ok_or(InspectError::Package(zup_bundle::PackageError::Invalid))?;
    let installer = &plan.installer;
    let file_size = std::fs::metadata(path)?.len();
    let target_matches_binary = match zup_binary::Executable::read(path) {
        Err(error) => format!("unreadable: {error}"),
        Ok(runtime) => match runtime.refuse_target(&installer.target) {
            Ok(()) => "matches".to_owned(),
            Err(error) => format!("mismatch: {error}"),
        },
    };
    let pin = zup_artifact::ArtifactPin::Pinned {
        version: installer.app.version.clone(),
    };
    Ok(Inspection {
        artifact: crate::automation::release_path(&crate::plain_path(path)),
        application: installer.app.name.to_string(),
        application_version: installer.app.version.to_string(),
        kind: zup_artifact::ArtifactKind::Single.as_str().to_owned(),
        mode: zup_artifact::ArtifactMode::Offline.as_str().to_owned(),
        pin: pin.label(),
        subsystem: zup_artifact::frontend_subsystem(installer.frontend)
            .as_str()
            .to_owned(),
        variants: vec![InspectedVariant {
            id: installer.target.to_string(),
            target: installer.target.to_string(),
            frontend: installer.frontend.as_str().to_owned(),
            logical_size: plan.total_size,
            file_count: plan.files.len() as u64,
            prerequisite_count: plan.prerequisites.len() as u64,
            plugin_count: plan.plugins.len() as u64,
            native_execution: false,
            target_matches_binary,
        }],
        content: InspectedContent {
            logical_size: plan.total_size,
            stored_size: plan.total_size,
            content_size: plan.total_size,
            shared_size: 0,
            exclusive_size: plan.total_size,
            unique_blob_count: carrier.package().blob_count() as u64,
            file_size,
        },
        trust: InspectedTrust {
            authenticode: "not applicable: a Linux installer carries no Authenticode structure"
                .to_owned(),
            index: "valid".to_owned(),
            content_digests: "valid".to_owned(),
            variants: "valid".to_owned(),
        },
    })
}

/// Every content digest is verified, so a report never describes content the
#[cfg(windows)]
fn inspect_composed(
    path: &std::path::Path,
    artifact: &UniversalArtifact,
) -> Result<Inspection, InspectError> {
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
    let carries_content = index.artifact.mode.carries_content();
    if carries_content {
        store.verify_all()?;
    }

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
            target_matches_binary: match artifact.view().variant_runtime(&variant.id) {
                Ok(None) => "carries no runtime".to_owned(),
                Ok(Some(bytes)) => match zup_binary::Executable::from_bytes(&bytes) {
                    Err(error) => format!("unreadable: {error}"),
                    Ok(runtime) => match runtime.refuse_target(&variant.target) {
                        Ok(()) => "matches".to_owned(),
                        Err(error) => format!("mismatch: {error}"),
                    },
                },
                Err(error) => format!("unreadable: {error}"),
            },
        })
        .collect();

    let authenticode = authenticode(path);
    let file_size = std::fs::metadata(path)?.len();
    Ok(Inspection {
        artifact: crate::automation::release_path(&crate::plain_path(path)),
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
            content_digests: if carries_content {
                "valid".to_owned()
            } else {
                "named".to_owned()
            },
            variants: "valid".to_owned(),
        },
    })
}

/// "digest matches" and never "signed" - a word that would be read as a claim
/// about a trust store this command never consulted.
#[cfg(windows)]
fn authenticode(path: &std::path::Path) -> String {
    let signature = match zup_pe::embedded_signature(path) {
        Ok(Some(signature)) => signature,
        Ok(None) => return "no certificate table".to_owned(),
        Err(_) => return "certificate table is malformed".to_owned(),
    };
    match zup_pe::image_digest(path) {
        Err(_) => "certificate table present, image digest unreadable".to_owned(),
        Ok(digest) if signature.digest.matches(&digest) => {
            format!("digest matches ({})", signature.digest.algorithm.as_str())
        }
        Ok(_) => format!(
            "DIGEST DOES NOT MATCH ({} over {})",
            signature.digest.algorithm.as_str(),
            signature.digest.to_hex()
        ),
    }
}

impl Inspection {
    pub fn human(&self) -> String {
        use zup_presentation::format_bytes;
        let mut out = String::new();
        out.push_str(&format!(
            "{} {}\n",
            self.application, self.application_version
        ));
        let linux = self.variants.iter().any(|variant| {
            variant
                .target
                .parse::<zup_core::TargetTriple>()
                .is_ok_and(|target| {
                    target.operating_system() == zup_core::TargetOperatingSystem::Linux
                })
        });
        let kind = match (self.kind.as_str(), self.mode.as_str(), linux) {
            ("universal", "offline", _) => "Windows universal offline installer".to_owned(),
            ("universal", "thin", _) => "Windows universal bootstrapper".to_owned(),
            ("single", "offline", true) => "Linux installer".to_owned(),
            ("single", "thin", true) => "Linux bootstrapper".to_owned(),
            ("single", "offline", false) => "Windows installer".to_owned(),
            ("single", "thin", false) => "Windows bootstrapper".to_owned(),
            (kind, mode, _) => format!("{kind} {mode} artifact"),
        };
        out.push_str(&kind);
        out.push_str(&format!("\n  pinned to    {}\n", self.pin));
        out.push_str("\nVariants\n");
        for variant in &self.variants {
            let binary = match variant.target_matches_binary.as_str() {
                "matches" | "carries no runtime" => String::new(),
                other => format!("  <- {other}"),
            };
            out.push_str(&format!(
                "  {:<24} {:<7}{}{}\n",
                variant.target,
                variant.frontend,
                if variant.native_execution {
                    "native only"
                } else {
                    "emulation allowed"
                },
                binary,
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
