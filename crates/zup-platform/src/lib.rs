//! Platform abstraction layer for zup.
//!
//! Platform-neutral target concepts. No Win32 handles or Windows API types
//! appear in these public APIs.

#![forbid(unsafe_code)]

mod command;
mod install_locations;
mod resolve;
mod target_path;
mod target_plan;

pub use command::CommandSpec;
pub use install_locations::{InstallLocationError, InstallLocationResolver};
pub use resolve::{TemplateResolveError, resolve_template_path};
pub use target_path::{TargetPath, TargetPathError};
pub use target_plan::{
    TargetFile, TargetFileAssociation, TargetLauncher, TargetPathEntry, TargetPlan,
    TargetPlanSummary, TargetPrerequisite, TargetProtocol, TargetService,
};

pub use zup_core::SelectedScope;
