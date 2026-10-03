//! What a plugin normally imports.
//!
//! The plugin's own vocabulary and the declarative resources it can return.
//! Nothing here is WebAssembly: a plugin author should not be able to tell from
//! the imports that the thing they are writing is compiled to Wasm.
//!
//! The two location and start enums are the WIT's own, because their variants
//! are what the host matches on and there is nothing to wrap.

pub use crate::plan::{Error, FileAssociation, Launcher, Path, Plan, Protocol, Service};
pub use crate::planner::{GeneratedFile, LauncherLocation, ServiceStart};
pub use crate::plugin::{Context, Plugin, Scope};
