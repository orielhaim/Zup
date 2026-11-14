#![forbid(unsafe_code)]

mod analysis;
mod error;
mod icons;
mod materialize;
mod pattern;
mod plugins;

pub use analysis::{PayloadAnalysis, PayloadBinary, analyze_payload};
pub use error::BuildError;
pub use materialize::{
    MAX_UPDATE_ROOT_BYTES, ResolvedUpdateRoot, Writes, materialize, materialize_destination,
    materialize_with_assets, materialize_with_policy, project_root, resolve_project_source,
    resolve_source_root, resolve_update_root,
};
pub use pattern::FilePattern;
