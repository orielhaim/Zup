//! What the host is *doing*, for a preset that presents more than the state.
//!
//! The prelude carries the state a person chose and the actions they can ask
//! for, which is the whole of what most presets draw. These are the rest of the
//! host's vocabulary: a plan being computed, a diagnostic, an operation in
//! progress, a maintenance surface. A preset that renders one of these is a
//! preset that has decided its window is a debug view, so reaching for this
//! module is a decision with a name attached rather than something the prelude
//! happens to have in it.
//!
//! Reached deliberately:
//!
//! ```ignore
//! use zup_sdk::preset::presentation::{DiagnosticKind, PlanStatus, ResourceCategory};
//! ```

pub use zup_preset_protocol::{
    ChangeGroup, ChangeKind, DiagnosticKind, DiagnosticPresentation, InstallOptions,
    InstallationHealth, LaunchTarget, OperationPhase, PlanPreview, PlanStatus, PlannedChange,
    ProgressPresentation, RequirementPresentation, RequirementStatus, ResourceCategory,
    UpdatePresentation, UpdateState,
};
