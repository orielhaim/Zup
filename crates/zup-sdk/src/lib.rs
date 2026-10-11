#![deny(unsafe_code)]

#[cfg(feature = "preset")]
pub mod preset;

#[cfg(feature = "plugin")]
pub mod plugin;

#[cfg(feature = "preset")]
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json;
}
