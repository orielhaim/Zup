//! Atomic build-document publication. Transaction durability is backend-owned.

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

/// Replace a document atomically after flushing its contents.
pub fn publish(path: &Path, contents: &[u8]) -> Result<(), DocumentError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| DocumentError::io(parent, source))?;
    atomic_write_file::AtomicWriteFile::open(path)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.commit()
        })
        .map_err(|source| DocumentError::io(path, source))
}

/// Refuse an existing document. Callers requiring exclusivity must hold a lock.
pub fn publish_new(path: &Path, contents: &[u8]) -> Result<(), DocumentError> {
    if path.symlink_metadata().is_ok() {
        return Err(DocumentError::Exists {
            path: path.to_path_buf(),
        });
    }
    publish(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(b"second".as_slice())]
    #[case(b"".as_slice())]
    fn publication_replaces_whole_document_and_cleans_up(#[case] contents: &[u8]) {
        let root = tempfile::tempdir().expect("temp directory");
        let path = root.path().join("nested/release.json");
        publish(&path, b"first").expect("first publish");
        publish(&path, contents).expect("replace");
        assert_eq!(std::fs::read(&path).expect("read"), contents);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap())
                .expect("directory")
                .count(),
            1
        );
    }

    #[test]
    fn publishing_new_refuses_to_replace() {
        let root = tempfile::tempdir().expect("temp directory");
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
}
