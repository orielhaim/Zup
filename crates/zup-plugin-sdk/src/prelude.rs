//! What a plugin normally imports.
//!
//! The plugin's own vocabulary and the declarative resources it can return.
//! Nothing here is WebAssembly: a plugin author should not be able to tell from
//! the imports that the thing they are writing is compiled to Wasm.

pub use crate::plan::{
    Error, FileAssociation, GeneratedFile, Launcher, LauncherLocation, Path, Plan, Protocol,
    Service, ServiceStart,
};
pub use crate::plugin::{Context, Plugin, Scope};
