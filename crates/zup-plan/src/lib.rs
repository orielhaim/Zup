//! Desired-state installation planner for zup.
//!
//! ```text
//! BuildPlan + PlanRequest → InstallPlan
//! ```
//!
//! Answers: *what should this installation contain for this scope/component
//! selection?* Execution delta planning lives in `zup-exec`.

#![forbid(unsafe_code)]

mod error;
mod plan;
mod plan_types;
mod request;
mod resolve;
mod resources;
mod select;

pub use error::{PlanError, SelectedScope};
pub use plan::plan;
pub use plan_types::{InstallPlan, PlanSummary};
pub use request::{ComponentOverrides, PlanRequest};
pub use resolve::{resolve_install_directory, resolve_template};
pub use resources::{
    PlannedExternalAction, PlannedFile, PlannedFileType, PlannedPathEntry, PlannedProtocol,
    PlannedService, PlannedShortcut,
};
pub use select::select_components;
pub use zup_core::ResourceKey;
