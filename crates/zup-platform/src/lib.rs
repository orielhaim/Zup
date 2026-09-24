//! Platform abstraction layer for zup.
//!
//! Platform-neutral target concepts. No Win32 handles or Windows API types
//! appear in these public APIs.

#![forbid(unsafe_code)]

mod command;
mod known_folders;
mod resolve;
mod target_path;
mod target_plan;

pub use command::CommandSpec;
pub use known_folders::{
    KnownFolder, KnownFolderError, KnownFolderResolver, known_folder_for_variable,
};
pub use resolve::{TemplateResolveError, resolve_template_path};
pub use target_path::{TargetPath, TargetPathError};
pub use target_plan::{
    TargetFile, TargetFileType, TargetPathEntry, TargetPlan, TargetPlanSummary, TargetProtocol,
    TargetService, TargetShortcut,
};

pub use zup_core::SelectedScope;
