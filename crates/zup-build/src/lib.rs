#![forbid(unsafe_code)]

mod digest;
mod error;
mod icons;
mod materialize;
mod pattern;
mod plugins;

pub use zup_core::{
    BuildPlan, ResolvedFile, ResolvedPlugin, ResolvedPrerequisite, TargetBuildPlan,
};

pub use digest::{DigestParseError, Sha256Digest};
pub use error::BuildError;
pub use materialize::{
    MAX_UPDATE_ROOT_BYTES, ResolvedUpdateRoot, Writes, materialize, materialize_destination,
    materialize_with_assets, materialize_with_policy, project_root, resolve_project_source,
    resolve_source_root, resolve_update_root,
};
pub use pattern::FilePattern;
pub use zup_bundle::MAX_PLUGIN_SOURCE_BYTES;
pub use zup_platform::{PortableSourceFilePolicy, SourceFilePolicy};
