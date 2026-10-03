//! `.zupui`: the portable artifact a preset is published as.
//!
//! One more artifact family, beside the installer package and the universal
//! artifact, with the same discipline: a fixed header, a canonical document, a
//! content-addressed store, and a reader that verifies rather than trusts.
//!
//! The split is by concern. `format` is what a package says, `package` is how it
//! is written and read, and this module is the boundary between them and the
//! rest of the crate.

mod format;
mod offers;
mod package;

pub use format::{
    HEADER_BYTES, MAGIC, MAX_BINARIES, MAX_BINARY_BYTES, MAX_METADATA_BYTES,
    MAX_TOTAL_BINARY_BYTES, PACKAGE_SCHEMA, PresetBinary, PresetBlob, PresetPackage,
    decode_metadata, encode_metadata,
};
pub use offers::{offers, offers_for};
pub use package::{PresetPackageView, PresetPackageWriter};
