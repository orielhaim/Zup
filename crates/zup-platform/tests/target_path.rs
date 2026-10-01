//! The cross-target path model: what a `TargetPath` accepts, how it spells
//! itself, and when two of them are the same location.
//!
//! Every case here is answered by the target's own rules, so the same test body
//! describes a Windows path on a Unix host and a Unix path on a Windows host.

use rstest::rstest;
use zup_core::{TargetOperatingSystem, TargetTriple};
use zup_platform::{TargetPath, TargetPathError};

fn windows() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn unix() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap()
}

/// One spelling rule for every root, however the target writes it.
#[rstest]
#[case::windows_drive_root(r"C:\", windows())]
#[case::windows_lowercase_drive_root(r"z:\", windows())]
#[case::windows_unc_root(r"\\server\share", windows())]
#[case::windows_forward_slash_unc_root(r"//server/share", windows())]
#[case::windows_unicode_unc_root(r"\\sérveur\partagé", windows())]
#[case::unix_root("/", unix())]
fn a_root_is_spelled_without_a_trailing_separator_except_where_it_needs_one(
    #[case] authored: &str,
    #[case] target: TargetTriple,
) {
    let expected = if target.operating_system() == TargetOperatingSystem::Windows {
        authored.replace('/', "\\")
    } else {
        authored.to_owned()
    };
    let root = TargetPath::new(&target, authored).unwrap();
    assert_eq!(root.as_str(), expected);
    assert_eq!(root.parent(), None, "path: {authored:?}");
    assert_eq!(root.file_name(), None, "path: {authored:?}");
}

/// Every separator the target accepts, spelled the one way a target path is
/// written.
#[rstest]
#[case::windows(r"C:\Program Files\Acme", r"C:/Program Files\Acme/", windows())]
#[case::windows_unc(r"\\server\share\Acme", r"//server/share/Acme", windows())]
#[case::windows_lowercase_drive(r"C:\pf", r"C:/pf", windows())]
#[case::unix("/opt/acme/bin", "/opt//acme/./bin/", unix())]
fn a_target_path_has_exactly_one_spelling(
    #[case] expected: &str,
    #[case] authored: &str,
    #[case] target: TargetTriple,
) {
    let path = TargetPath::new(&target, authored).unwrap();
    assert_eq!(path.as_str(), expected);
    // The canonical form is a fixed point: re-binding it changes nothing.
    assert_eq!(
        TargetPath::new(&target, path.as_str()).unwrap().as_str(),
        expected
    );
}

/// A `\` is a literal character on a Unix target, so it cannot separate
/// anything and cannot make one path a prefix of another.
#[test]
fn a_unix_backslash_is_part_of_the_name() {
    let base = TargetPath::new(unix(), "/opt/acme").unwrap();
    let path = TargetPath::new(unix(), r"/opt/acme\tool").unwrap();
    assert_eq!(path.as_str(), r"/opt/acme\tool");
    assert_eq!(path.file_name(), Some(r"acme\tool"));
    assert!(!path.starts_with(&base));
    assert!(!base.starts_with(&path));
}

/// A path is only below another path by whole components, so `C:\PFX` is not
/// below `C:\PF`.
#[rstest]
#[case::windows_sibling(r"C:\PF", r"C:\PFX", windows())]
#[case::windows_unc_sibling(r"\\server\share\Acme", r"\\server\share\AcmeX", windows())]
#[case::unix_sibling("/opt/acme", "/opt/acmeX", unix())]
fn a_prefix_is_whole_components_only(
    #[case] base: &str,
    #[case] path: &str,
    #[case] target: TargetTriple,
) {
    let base = TargetPath::new(&target, base).unwrap();
    let path = TargetPath::new(&target, path).unwrap();
    assert!(
        !path.starts_with(&base),
        "{path:?} should not start with {base:?}"
    );
    assert!(base.starts_with(&base));
    assert!(base.equivalent(&base));
}

/// Windows spells paths case-insensitively and either way round; a Unix target
/// compares bytes.
#[rstest]
#[case::windows_case_and_separator(r"C:\Apps\Acme", r"c:/apps/acme", windows())]
#[case::windows_unc_case(r"\\server\share\Acme", r"\\SERVER\SHARE\acme", windows())]
fn a_windows_path_folds_case(
    #[case] left: &str,
    #[case] right: &str,
    #[case] target: TargetTriple,
) {
    let left = TargetPath::new(&target, left).unwrap();
    let right = TargetPath::new(&target, right).unwrap();
    assert!(left.equivalent(&right), "{left:?} vs {right:?}");
}

#[test]
fn a_unix_path_compares_bytes() {
    assert!(
        !TargetPath::new(unix(), "/opt/Acme")
            .unwrap()
            .equivalent(&TargetPath::new(unix(), "/opt/acme").unwrap())
    );
}

/// Two targets never share a location, even when the text matches.
#[test]
fn a_path_is_only_equivalent_within_its_own_target() {
    let windows_path = TargetPath::new(windows(), r"C:\PF").unwrap();
    let other_windows = TargetTriple::parse("aarch64-pc-windows-msvc").unwrap();
    let other = TargetPath::new(&other_windows, r"C:\PF").unwrap();
    assert!(!windows_path.equivalent(&other));
    assert!(!windows_path.starts_with(&other));
}

/// A path is bound to its target, so a Windows path is parsed as Windows no
/// matter which machine is doing the parsing.
#[rstest]
#[case::windows(r"C:\Acme\toolé", r"C:\Acme\toolé")]
#[case::unc(r"\\server\share\Acme", r"\\server\share\Acme")]
#[case::unix("/opt/café/naïvé", "/opt/café/naïvé")]
fn a_target_path_names_its_own_target_not_the_host(#[case] authored: &str, #[case] expected: &str) {
    let windows_target = authored.contains('\\');
    let target = if windows_target { windows() } else { unix() };
    let path = TargetPath::new(&target, authored).unwrap();
    assert_eq!(path.as_str(), expected);
    assert_eq!(path.target(), &target);
}

/// Multi-byte characters end a component, so a parent is found by separator
/// rather than by byte offset.
#[rstest]
#[case::windows(r"C:\Ünïcodé\toolé", &[r"C:\Ünïcodé", r"C:\"])]
#[case::windows_unc(r"\\sérveur\partagé\toolé", &[r"\\sérveur\partagé"])]
#[case::unix("/opt/café/naïvé", &["/opt/café", "/opt", "/"])]
fn a_parent_chain_ends_at_the_root(#[case] authored: &str, #[case] ancestors: &[&str]) {
    let target = if authored.contains('\\') {
        windows()
    } else {
        unix()
    };
    let path = TargetPath::new(&target, authored).unwrap();
    assert_eq!(
        path.file_name(),
        ancestors.first().map(|_| path.file_name().unwrap())
    );
    let mut at = path.clone();
    for expected in ancestors {
        at = at.parent().expect("the path is above its root");
        assert_eq!(at.as_str(), *expected);
    }
    assert_eq!(at.parent(), None);
}

/// `join` extends a path in place; it never relocates it and never climbs out.
#[rstest]
#[case::name(r"C:\PF", "MixedCase", Ok(r"C:\PF\MixedCase"))]
#[case::nested(r"C:\PF", r"bin\tool.exe", Ok(r"C:\PF\bin\tool.exe"))]
#[case::below_root(r"C:\", "x", Ok(r"C:\x"))]
#[case::below_share(r"\\server\share", "Acme", Ok(r"\\server\share\Acme"))]
#[case::unix("/", "opt", Ok("/opt"))]
#[case::unix_nested("/", "opt/acme", Ok("/opt/acme"))]
#[case::empty(r"C:\PF", "", Ok(r"C:\PF"))]
#[case::other_drive(r"C:\PF", r"D:\Other", Err(()))]
#[case::rooted(r"C:\PF", r"\Other", Err(()))]
#[case::drive_relative(r"C:\PF", "C:tool", Err(()))]
#[case::parent(r"C:\PF", "..", Err(()))]
#[case::nested_parent(r"C:\PF", r"bin\..", Err(()))]
#[case::unresolved(r"C:\PF", "${install}", Err(()))]
fn joining_extends_a_target_path(
    #[case] base: &str,
    #[case] suffix: &str,
    #[case] expected: Result<&str, ()>,
) {
    let target = if base.contains('\\') {
        windows()
    } else {
        unix()
    };
    let base = TargetPath::new(&target, base).unwrap();
    match (base.join(suffix), expected) {
        (Ok(joined), Ok(expected)) => assert_eq!(joined.as_str(), expected),
        (Ok(joined), Err(())) => panic!("{suffix:?} should not join: {joined:?}"),
        (Err(error), Err(())) => assert!(
            matches!(
                error,
                TargetPathError::Traversal { .. }
                    | TargetPathError::InvalidComponent { .. }
                    | TargetPathError::UnresolvedVariable { .. }
            ),
            "{suffix:?}: {error:?}"
        ),
        (Err(error), Ok(expected)) => panic!("{suffix:?} should join to {expected:?}: {error:?}"),
    }
}

/// A suffix that names a drive of its own is a location, not an extension.
#[test]
fn a_unix_suffix_may_name_a_windows_drive() {
    assert_eq!(
        TargetPath::new(unix(), "/opt/acme")
            .unwrap()
            .join("C:tool")
            .unwrap()
            .as_str(),
        "/opt/acme/C:tool"
    );
}

/// An authored path that does not name a location the target can install to.
#[rstest]
#[case::windows_no_prefix(r"\Acme", windows())]
#[case::windows_drive_relative(r"C:Acme", windows())]
#[case::windows_relative(r"Acme\tool.exe", windows())]
#[case::unix_relative(r"opt/acme", unix())]
#[case::unix_relative_drive(r"C:\Acme", unix())]
#[case::windows_relative_unix(r"/opt/acme", windows())]
fn a_relative_path_is_refused(#[case] authored: &str, #[case] target: TargetTriple) {
    assert!(
        matches!(
            TargetPath::new(&target, authored),
            Err(TargetPathError::NotAbsolute { .. })
        ),
        "{authored:?}"
    );
}

/// Windows device namespaces name kernel objects, so they are not places an
/// installer can write.
#[rstest]
#[case::verbatim_disk(r"\\?\C:\Windows")]
#[case::verbatim_unc(r"\\?\UNC\server\share")]
#[case::verbatim_relative(r"\\?\pictures")]
#[case::device_ns(r"\\.\PIPE\device")]
#[case::nt_device(r"\\??\C:\Windows")]
#[case::unc_dot_server(r"\\server\.\Acme")]
#[case::unc_parent_server(r"\\..\share\Acme")]
fn a_device_namespace_is_refused(#[case] authored: &str) {
    assert!(
        matches!(
            TargetPath::new(windows(), authored),
            Err(TargetPathError::DevicePath { .. })
        ),
        "{authored:?}"
    );
}

/// `..` names a directory outside the one that was spelled; `.` names the same
/// directory, so the canonical form absorbs it.
#[rstest]
#[case::windows(r"C:\PF\..\Windows")]
#[case::windows_root(r"C:\..\Windows")]
#[case::windows_unc(r"\\server\share\..\Acme")]
#[case::unix("/opt/../acme")]
fn a_parent_component_is_refused(#[case] authored: &str) {
    let target = if authored.contains('\\') {
        windows()
    } else {
        unix()
    };
    assert!(
        matches!(
            TargetPath::new(&target, authored),
            Err(TargetPathError::Traversal { .. })
        ),
        "{authored:?}"
    );
}

#[rstest]
#[case::windows(r"C:\PF\.\Windows", r"C:\PF\Windows")]
#[case::windows_trailing(r"C:\PF\.", r"C:\PF")]
#[case::unix("/opt/./acme", "/opt/acme")]
fn a_current_component_is_absorbed(#[case] authored: &str, #[case] expected: &str) {
    let target = if authored.contains('\\') {
        windows()
    } else {
        unix()
    };
    assert_eq!(
        TargetPath::new(&target, authored).unwrap().as_str(),
        expected
    );
}

/// A target path carries its target, so an unresolvable or rebuilt ledger entry
/// is caught at the boundary rather than written out.
#[test]
fn a_target_path_round_trips_through_its_serialized_form() {
    for authored in [
        r"C:\Program Files\Acme\app.exe",
        r"\\server\share\Acme",
        r"C:\",
        "/opt/acme/bin",
        "/",
    ] {
        let target = if authored.contains('\\') {
            windows()
        } else {
            unix()
        };
        let path = TargetPath::new(&target, authored).unwrap();
        let json = serde_json::to_string(&path).unwrap();
        let restored: TargetPath = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, path, "{authored:?}");
        assert_eq!(restored.as_str(), path.as_str(), "{authored:?}");
    }
}

#[test]
fn a_serialized_target_path_without_its_target_is_refused() {
    assert!(
        serde_json::from_str::<TargetPath>(r#"{"path":"C:\\PF"}"#).is_err(),
        "a path with no target cannot be bound to one"
    );
    assert!(
        serde_json::from_str::<TargetPath>(
            r#"{"target":"x86_64-pc-windows-msvc","path":"/opt/acme"}"#
        )
        .is_err(),
        "a Unix path is not absolute on a Windows target"
    );
}
