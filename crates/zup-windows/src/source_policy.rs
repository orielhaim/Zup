//! Windows adapter for the build source-inspection seam.
//!
//! `std::fs` reports a Windows entry as a symlink only when its reparse tag is a
//! name surrogate, which covers a symbolic link and a directory junction but not
//! a cloud placeholder, a deduplicated entry, or any other reparse point that
//! presents as a plain file and is read straight through. This adapter refuses
//! every entry carrying `FILE_ATTRIBUTE_REPARSE_POINT` — the same attribute the
//! bootstrap filesystem seam uses to refuse a write through one.

use std::io;
use std::path::Path;

use zup_build::SourceFilePolicy;

use crate::fs_bindings;

/// The source policy a Windows build must inject: symlink metadata plus
/// `FILE_ATTRIBUTE_REPARSE_POINT`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsSourceFilePolicy;

impl SourceFilePolicy for WindowsSourceFilePolicy {
    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        match path.symlink_metadata() {
            Ok(metadata) => {
                Ok(metadata.file_type().is_symlink() || fs_bindings::is_reparse_point(path))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
