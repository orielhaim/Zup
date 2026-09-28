//! Unit tests for portable relative paths.

use std::path::Path;

use rstest::rstest;
use zup_core::{RelativePath, RelativePathError};

/// Separators are normalized to `/` on the way in, so a Windows-authored path
/// is byte-identical to its POSIX spelling.
#[rstest]
#[case::nested("bin/helpers/foo.dll", "bin/helpers/foo.dll")]
#[case::backslashes("bin\\helpers\\foo.dll", "bin/helpers/foo.dll")]
fn paths_normalize_their_separators(#[case] source: &str, #[case] expected: &str) {
    let path = RelativePath::new(source).unwrap();
    assert_eq!(path.as_str(), expected);
    assert_eq!(path.to_string(), expected);
}

#[test]
fn file_name_and_parent() {
    let path = RelativePath::new("bin/helpers/foo.dll").unwrap();
    assert_eq!(path.file_name(), "foo.dll");
    assert_eq!(path.parent().unwrap().as_str(), "bin/helpers");
    assert_eq!(path.component_count(), 3);
}

/// An absolute or traversing path must be refused at every entry point, including
/// the `Path` adapter, or a caller can smuggle one in through `OsStr`.
#[rstest]
#[case::empty("")]
#[case::absolute("/etc/passwd")]
#[case::parent("../secret")]
#[case::embedded_parent("a/../b")]
#[case::double_slash("a//b")]
fn invalid_paths(#[case] source: &str) {
    assert!(RelativePath::new(source).is_err(), "source: {source}");
}

#[test]
fn from_path_rejects_absolute_and_parent() {
    assert!(matches!(
        RelativePath::from_path(Path::new("/abs")),
        Err(RelativePathError::Absolute { .. })
    ));
    assert!(matches!(
        RelativePath::from_path(Path::new("a/../b")),
        Err(RelativePathError::ParentTraversal { .. })
    ));
    assert!(RelativePath::from_path(Path::new("a/b.txt")).is_ok());
}
