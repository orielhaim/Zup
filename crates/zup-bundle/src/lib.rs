//! Payload and container layer for zup.
//!
//! Portable payload access by `RelativePath` + expected digest. The final
//! `.zup` bundle/container format is not implemented yet.

#![forbid(unsafe_code)]

mod payload;

pub use payload::{DirectoryPayloadSource, PayloadError, PayloadReader, PayloadSource};
