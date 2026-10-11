//! Publishing a document so that no reader ever observes half of one.
//!
//! A build pipeline writes several documents that something else reads
//! afterwards: the release description, a signing plan, a signing manifest. Each
//! one is read by a process that did not write it and did not choose when it
//! starts, so the only property that matters is that a reader sees either the
//! previous document or the complete new one, never a prefix.
//!
//! That is a property of a filesystem, not of a platform, and it is the same
//! property on both of this repository's backends: write to a temporary file in
//! the destination's own directory, flush it, then rename it into place. The
//! rename is atomic, so a reader cannot observe the interval.
//!
//! # What this does and does not guarantee
//!
//! It guarantees that a *reader* never observes a partial document, and that the
//! document's bytes are on the device before the name referring to them exists -
//! `atomic-write-file` flushes the file before it renames it, which is the order
//! the guarantee depends on.
//!
//! It does *not* guarantee that the rename survives power loss. That needs the
//! parent directory entry flushed too, which needs a mechanism a portable crate
//! has no honest way to reach: `zup-windows` has `MoveFileExW` with
//! `MOVEFILE_WRITE_THROUGH`, and a Linux backend would need the directory `fsync`
//! that only a native layer can state. That guarantee belongs to the backend that
//! owns the durability contract, and this module says so rather than pretending a
//! portable call met it. A transaction journal's recovery semantics are stated in
//! terms of power loss, so the journal does not use this; a build pipeline
//! publishing a release description to a directory nobody else is writing does.
//!
//! No `unsafe` is involved, and none is available to be involved: publication is a
//! rename of a flushed temporary, which `atomic-write-file` already implements.

use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Failures produced while publishing a document.
#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("document `{path}` already exists")]
    Exists { path: PathBuf },
    #[error("document `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl DocumentError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Publish `contents` at `path`, replacing whatever is there.
///
/// The destination's directory is created if it is missing, so a caller writing
/// the first document of a fresh release does not have to know that yet.
pub fn publish(path: &Path, contents: &[u8]) -> Result<(), DocumentError> {
    write(path, contents)
}

/// Publish `contents` at `path`, refusing to replace an existing document.
///
/// The difference matters for anything whose *absence* is meaningful: a signing
/// plan that already exists is a plan somebody already approved, and overwriting
/// it because a second build ran would be a different document from the one the
/// approval was given for.
///
/// The refusal is a check, not a lock: a document published between the check and
/// the rename is overwritten. That is stated because it is true. A caller that
/// needs exclusivity takes a lock - which is `zup-transaction`'s
/// `InstallationLock` - rather than inferring it from a function that cannot
/// promise it.
pub fn publish_new(path: &Path, contents: &[u8]) -> Result<(), DocumentError> {
    if path.symlink_metadata().is_ok() {
        return Err(DocumentError::Exists {
            path: path.to_path_buf(),
        });
    }
    write(path, contents)
}

fn write(path: &Path, contents: &[u8]) -> Result<(), DocumentError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| DocumentError::io(parent, source))?;

    // `atomic-write-file` owns the temporary, the flush, and the rename rather than
    // this module re-implementing them. Depending on it is the point: the atomic
    // publication primitive is one implementation in the workspace rather than two
    // that agree today.
    atomic_write_file::AtomicWriteFile::open(path)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.commit()
        })
        .map_err(|source| DocumentError::io(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp directory")
    }

    /// The whole point of the primitive. A reader either finds the previous
    /// document or the complete new one, and never a prefix - which is exactly
    /// what a write-and-truncate produces when the process dies mid-write.
    #[test]
    fn publishing_replaces_a_document_wholesale() {
        let root = temp();
        let path = root.path().join("nested").join("release.json");
        publish(&path, b"first").expect("first publish");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "first");

        publish(&path, b"second").expect("second publish");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "second",
            "a second publication replaces the first rather than appending to it"
        );
    }

    /// `publish_new` exists for documents whose *absence* is meaningful, so it has
    /// to refuse rather than overwrite - and it has to leave the existing document
    /// exactly as it was when it refuses.
    #[test]
    fn publishing_new_refuses_to_replace() {
        let root = temp();
        let path = root.path().join("signing-plan.json");
        publish(&path, b"approved").expect("first publish");

        let refused = publish_new(&path, b"different").expect_err("a second publish is refused");
        assert!(matches!(refused, DocumentError::Exists { .. }), "{refused}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "approved",
            "a refusal must leave the existing document untouched"
        );
    }

    /// A publication leaves nothing behind. A temporary file beside a published
    /// document would be read by anything that enumerates the directory, and the
    /// first document of a release is written precisely when that directory is
    /// being watched.
    #[test]
    fn no_temporary_survives_a_publication() {
        let root = temp();
        let path = root.path().join("release.json");
        publish(&path, b"contents").expect("publish");
        let entries = std::fs::read_dir(root.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec!["release.json"], "{entries:?}");
    }

    /// The parent is created, so a caller writing the first document of a release
    /// does not have to know that the directory does not exist yet.
    #[test]
    fn the_destination_directory_is_created() {
        let root = temp();
        let path = root.path().join("a").join("b").join("release.json");
        publish(&path, b"contents").expect("publish");
        assert!(path.is_file(), "{} should exist", path.display());
    }

    /// An empty document is a document. A caller writing zero bytes is writing
    /// something, and a publication that refused would be surprising rather than
    /// safe.
    #[test]
    fn an_empty_document_publishes() {
        let root = temp();
        let path = root.path().join("empty.json");
        publish(&path, b"").expect("publish");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "");
    }
}
