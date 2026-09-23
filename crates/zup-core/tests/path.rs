//! Unit tests for portable relative paths.

use std::path::Path;

use rstest::rstest;
use zup_core::{RelativePath, RelativePathError};

#[rstest]
#[case::simple("acme.exe")]
#[case::nested("bin/helpers/foo.dll")]
#[case::backslashes("bin\\helpers\\foo.dll")]
fn valid_paths(#[case] source: &str) {
    let path = RelativePath::new(source).unwrap();
    assert!(!path.as_str().contains('\\'));
    assert!(!path.as_str().contains("//"));
}

#[test]
fn display_uses_forward_slashes() {
    let path = RelativePath::new("bin\\a\\b.txt").unwrap();
    assert_eq!(path.as_str(), "bin/a/b.txt");
    assert_eq!(path.to_string(), "bin/a/b.txt");
}

#[test]
fn file_name_and_parent() {
    let path = RelativePath::new("bin/helpers/foo.dll").unwrap();
    assert_eq!(path.file_name(), "foo.dll");
    assert_eq!(path.parent().unwrap().as_str(), "bin/helpers");
    assert_eq!(path.component_count(), 3);
}

#[test]
fn join() {
    let a = RelativePath::new("bin").unwrap();
    let b = RelativePath::new("helpers/foo.dll").unwrap();
    assert_eq!(a.join(&b).as_str(), "bin/helpers/foo.dll");
}

#[rstest]
#[case::empty("")]
#[case::absolute("/etc/passwd")]
#[case::parent("../secret")]
#[case::embedded_parent("a/../b")]
#[case::trailing_slash("a/")]
#[case::leading_slash("/a")]
#[case::dot_component("a/./b")]
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
