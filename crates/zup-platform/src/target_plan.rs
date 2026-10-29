//! Desired state resolved for one concrete target machine.

use serde::{Deserialize, Serialize};
use zup_core::{
    App, ComponentId, FileAssociationId, FileExtension, InstalledPreset, LauncherLocation,
    NonEmptyString, PrerequisitePackage, Privilege, ProtocolScheme, RelativePath, ResourceKey,
    SelectedScope, ServiceId, ServiceStart, Sha256Digest, Target,
};
use zup_plan::PlannedPrerequisite;

use crate::command::CommandSpec;
use crate::target_path::TargetPath;
use thiserror::Error;

/// Target-resolved plan. Contains no `Template` values and no build-machine paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlan {
    pub app: App,
    pub target: Target,
    pub scope: SelectedScope,
    pub install_directory: TargetPath,
    pub selected_components: Vec<ComponentId>,

    #[serde(default)]
    pub prerequisites: Vec<PlannedPrerequisite>,
    pub files: Vec<TargetFile>,
    pub launchers: Vec<TargetLauncher>,
    pub path_entries: Vec<TargetPathEntry>,
    pub services: Vec<TargetService>,
    pub protocols: Vec<TargetProtocol>,
    pub file_associations: Vec<TargetFileAssociation>,

    pub summary: TargetPlanSummary,
    /// The preset runtime this target will make durable, or none for a target that
    /// presents no window.
    ///
    /// Named here rather than inferred from the files below, because the files
    /// are content and this is the record of what they are for. An installation
    /// whose window cannot be reconstructed from its own state is not installed.
    pub preset: Option<InstalledPreset>,
}

/// One desired payload file on the target machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFile {
    pub key: ResourceKey,
    pub source_relative: RelativePath,
    pub destination: TargetPath,
    pub size: u64,
    pub sha256: Sha256Digest,
    pub privilege: Privilege,
    /// This file is intended to be executable. Portable intent, not a mode.
    #[serde(default)]
    pub executable: bool,
}

/// One desired application launcher with a concrete launcher path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetLauncher {
    pub key: ResourceKey,
    pub location: LauncherLocation,
    pub name: NonEmptyString,
    /// Concrete launcher path on the target machine.
    pub launcher_path: TargetPath,
    pub target: TargetPath,
    pub arguments: Vec<String>,
    pub working_directory: Option<TargetPath>,
    pub privilege: Privilege,
}

/// One desired search-path entry.
///
/// `scope` names the persistent search path that owns the entry (the per-user
/// or host-wide one). It is an independent decision from `privilege`: a
/// host-wide install may still add a per-user entry, and vice versa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPathEntry {
    pub key: ResourceKey,
    pub value: TargetPath,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// One desired service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetService {
    pub key: ResourceKey,
    pub id: ServiceId,
    pub name: NonEmptyString,
    pub display_name: Option<NonEmptyString>,
    pub command: CommandSpec,
    pub start: ServiceStart,
    pub privilege: Privilege,
}

/// One desired URI-protocol registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetProtocol {
    pub key: ResourceKey,
    pub scheme: ProtocolScheme,
    pub command: CommandSpec,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// One desired file-association registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFileAssociation {
    pub key: ResourceKey,
    pub extension: FileExtension,
    pub id: FileAssociationId,
    pub description: Option<String>,
    pub command: CommandSpec,
    pub scope: SelectedScope,
    pub privilege: Privilege,
}

/// Derived summary of a target plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPlanSummary {
    pub file_count: usize,
    pub install_bytes: u64,
    pub resource_count: usize,
    /// True when at least one planned resource needs system authorization.
    pub requires_authorization: bool,
    pub selected_component_count: usize,
    pub prerequisite_count: usize,
    pub download_bytes: u64,
}

/// The common file and prerequisite portion lowered from a resolved install plan.
/// Platform backends remain responsible for validating each target path and checking
/// collisions before accepting these files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPlanFiles {
    prerequisites: Vec<PlannedPrerequisite>,
    files: Vec<TargetFile>,
    install_bytes: u64,
    download_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("target plan size overflow")]
pub struct TargetPlanSizeOverflow;

impl TargetPlanFiles {
    pub fn files(&self) -> &[TargetFile] {
        &self.files
    }

    pub const fn install_bytes(&self) -> u64 {
        self.install_bytes
    }

    pub const fn download_bytes(&self) -> u64 {
        self.download_bytes
    }

    pub fn into_parts(self) -> (Vec<PlannedPrerequisite>, Vec<TargetFile>) {
        (self.prerequisites, self.files)
    }

    /// Add a backend-generated file while keeping the shared byte total checked.
    pub fn push_file(&mut self, file: TargetFile) -> Result<(), TargetPlanSizeOverflow> {
        self.install_bytes = self
            .install_bytes
            .checked_add(file.size)
            .ok_or(TargetPlanSizeOverflow)?;
        self.files.push(file);
        Ok(())
    }
}

#[derive(Debug)]
pub enum TargetPlanFilesError<E> {
    Resolve(E),
    SizeOverflow,
}

/// Lower the common payload files and prerequisites shared by platform backends.
/// The resolver callback performs platform-specific template and path validation.
pub fn lower_target_plan_files<E>(
    plan: &zup_plan::InstallPlan,
    mut resolve: impl FnMut(&zup_core::Template, &'static str) -> Result<TargetPath, E>,
) -> Result<TargetPlanFiles, TargetPlanFilesError<E>> {
    let prerequisites = plan.prerequisites.clone();

    let mut files = TargetPlanFiles {
        prerequisites,
        files: Vec::with_capacity(plan.files.len()),
        install_bytes: 0,
        download_bytes: 0,
    };
    files.download_bytes = files
        .prerequisites
        .iter()
        .filter_map(|prerequisite| match &prerequisite.package {
            PrerequisitePackage::Remote { size, .. } => *size,
            PrerequisitePackage::Embedded { .. } => None,
        })
        .try_fold(0u64, |total, size| total.checked_add(size))
        .ok_or(TargetPlanFilesError::SizeOverflow)?;
    for file in &plan.files {
        let destination = resolve(&file.destination, "file destination")
            .map_err(TargetPlanFilesError::Resolve)?;
        files
            .push_file(TargetFile {
                key: ResourceKey::File {
                    destination: destination.to_string(),
                },
                source_relative: file.source_relative.clone(),
                destination,
                size: file.size,
                sha256: file.sha256,
                executable: file.executable,
                privilege: file.privilege,
            })
            .map_err(|_| TargetPlanFilesError::SizeOverflow)?;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use zup_core::{
        App, AppId, NonEmptyString, PrerequisiteArchitecture, PrerequisiteId,
        PrerequisiteRequirement, Runtime, RuntimeRequirementId, SelectedScope, Target, Template,
        hash_bytes,
    };
    use zup_plan::{InstallPlan, PlanSummary, PlannedFile, PlannedPrerequisite};

    fn plan() -> InstallPlan {
        InstallPlan {
            app: App {
                id: AppId::new("com.example.app").unwrap(),
                name: NonEmptyString::new("App").unwrap(),
                version: Version::new(1, 0, 0),
                publisher: None,
                main: None,
                description: None,
            },
            target: Target::parse("linux-x64").unwrap(),
            scope: SelectedScope::User,
            install_directory: Template::parse("/opt/app").unwrap(),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: Vec::new(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: PlanSummary {
                file_count: 0,
                install_bytes: 0,
                selected_component_count: 0,
                resource_count: 0,
                requires_authorization: false,
                prerequisite_count: 0,
                download_bytes: 0,
            },
        }
    }

    fn remote_prerequisite(name: &str, size: Option<u64>) -> PlannedPrerequisite {
        PlannedPrerequisite {
            id: PrerequisiteId::new(name).unwrap(),
            name: NonEmptyString::new(name).unwrap(),
            target: PrerequisiteArchitecture::Any,
            requirement: PrerequisiteRequirement::Runtime(Runtime {
                id: RuntimeRequirementId::new("test.runtime").unwrap(),
                version: None,
            }),
            package: PrerequisitePackage::Remote {
                url: "https://example.invalid/package".into(),
                sha256: hash_bytes(b"package"),
                size,
                filename: "package.bin".into(),
            },
            installer: Default::default(),
        }
    }

    #[test]
    fn payload_and_generated_files_share_checked_install_total() {
        let mut plan = plan();
        plan.files.push(PlannedFile {
            key: ResourceKey::File {
                destination: "/opt/app/payload".into(),
            },
            source_relative: zup_core::RelativePath::new("payload").unwrap(),
            destination: Template::parse("/opt/app/payload").unwrap(),
            size: u64::MAX,
            sha256: hash_bytes(b"payload"),
            privilege: zup_core::Privilege::User,
            executable: false,
        });
        let mut lowered = lower_target_plan_files(&plan, |_, _| {
            TargetPath::new(plan.target, "/opt/app/payload").map_err(|error| error.to_string())
        })
        .unwrap();

        let generated = TargetFile {
            key: ResourceKey::File {
                destination: "/opt/app/generated.desktop".into(),
            },
            source_relative: zup_core::RelativePath::new("generated.desktop").unwrap(),
            destination: TargetPath::new(plan.target, "/opt/app/generated.desktop").unwrap(),
            size: 1,
            sha256: hash_bytes(b"generated"),
            privilege: zup_core::Privilege::User,
            executable: false,
        };
        assert_eq!(lowered.install_bytes(), u64::MAX);
        assert_eq!(lowered.push_file(generated), Err(TargetPlanSizeOverflow));
        assert_eq!(lowered.install_bytes(), u64::MAX);
        assert_eq!(lowered.files().len(), 1);
    }

    #[test]
    fn remote_download_sizes_use_checked_total() {
        let mut plan = plan();
        plan.prerequisites
            .push(remote_prerequisite("first", Some(u64::MAX)));
        plan.prerequisites
            .push(remote_prerequisite("second", Some(1)));
        assert!(matches!(
            lower_target_plan_files(&plan, |_, _| -> Result<TargetPath, String> {
                unreachable!()
            }),
            Err(TargetPlanFilesError::SizeOverflow)
        ));
    }
}
