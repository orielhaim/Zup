use std::collections::BTreeMap;

use rstest::rstest;
use zup_core::{InstallLocation, SelectedScope, TargetTriple, Template, Variable, VariableValue};
use zup_platform::{
    InstallLocationError, InstallLocationResolver, TargetPath, TargetPathError,
    TemplateResolveError, resolve_template_path,
};

#[derive(Debug, Default, Clone)]
struct FakeLocations {
    paths: BTreeMap<InstallLocation, String>,
}

impl FakeLocations {
    fn resolved(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let path =
            self.paths
                .get(&location)
                .cloned()
                .ok_or(InstallLocationError::ResolutionFailed {
                    location,
                    scope,
                    source: "not configured".into(),
                })?;
        TargetPath::new(target, &path).map_err(|source| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: source.into(),
        })
    }
}

impl InstallLocationResolver for FakeLocations {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        self.resolved(location, scope, target)
    }
}

/// Answers with paths that belong to one target per location regardless of the
/// target it was asked for, so target drift surfaces as a resolver fault.
#[derive(Debug, Clone)]
struct DriftingTargetLocations;

impl InstallLocationResolver for DriftingTargetLocations {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        _target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let (owning, path) = match location {
            InstallLocation::Programs => (windows_target(), r"C:\PF"),
            _ => (unix_target(), "/opt/pf"),
        };
        TargetPath::new(&owning, path).map_err(|source| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: source.into(),
        })
    }
}

fn windows_target() -> TargetTriple {
    TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
}

fn unix_target() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap()
}

fn fake_locations() -> FakeLocations {
    let mut paths = BTreeMap::new();
    paths.insert(InstallLocation::Programs, r"C:\PF".to_owned());
    paths.insert(
        InstallLocation::UserData,
        r"C:\Users\Test\AppData\Local".to_owned(),
    );
    paths.insert(InstallLocation::SharedData, r"C:\ProgramData".to_owned());
    paths.insert(
        InstallLocation::Menu,
        r"C:\Users\Test\Start Menu".to_owned(),
    );
    paths.insert(
        InstallLocation::Desktop,
        r"C:\Users\Test\Desktop".to_owned(),
    );
    FakeLocations { paths }
}

#[rstest]
#[case::windows(r"C:\Windows\System32\cmd.exe", r"C:\Windows\System32\cmd.exe")]
#[case::windows_forward_slash(r"C:/Windows/System32/cmd.exe", r"C:\Windows\System32\cmd.exe")]
#[case::windows_unc(r"\\server\share\Acme", r"\\server\share\Acme")]
fn accepts_windows_absolute(#[case] raw: &str, #[case] expected: &str) {
    let path = TargetPath::new(windows_target(), raw).unwrap();
    assert_eq!(path.as_str(), expected);
    assert_eq!(path.target(), &windows_target());
}

#[test]
fn accepts_unix_absolute_without_host_inference() {
    let path = TargetPath::new(unix_target(), "/opt/acme/bin").unwrap();
    assert_eq!(path.as_str(), "/opt/acme/bin");
    assert_eq!(path.target(), &unix_target());
}

#[test]
fn unc_paths_keep_their_root_and_parent() {
    let path = TargetPath::new(windows_target(), r"\\server\share\Acme\tool.exe").unwrap();
    assert_eq!(path.as_str(), r"\\server\share\Acme\tool.exe");
    assert_eq!(path.parent().unwrap().as_str(), r"\\server\share\Acme");
    assert_eq!(
        path.parent().unwrap().parent().unwrap().as_str(),
        r"\\server\share"
    );
    assert_eq!(path.file_name(), Some("tool.exe"));
}

/// A path whose segments end on multi-byte characters: the byte before the end
/// is not a character boundary, so the split has to be on separators.
#[rstest]
#[case::windows(r"C:\Ünïcodé\toolé", "windows", &["C:\\Ünïcodé", "C:\\"], "toolé")]
#[case::unix("/opt/café/naïvé", "unix", &["/opt/café", "/opt", "/"], "naïvé")]
fn parent_splits_on_separators_not_bytes(
    #[case] authored: &str,
    #[case] target_kind: &str,
    #[case] ancestors: &[&str],
    #[case] file_name: &str,
) {
    let target = if target_kind == "unix" {
        unix_target()
    } else {
        windows_target()
    };
    let path = TargetPath::new(target, authored).unwrap();
    assert_eq!(path.as_str(), authored);
    assert_eq!(path.file_name(), Some(file_name));
    for (depth, expected) in ancestors.iter().enumerate() {
        let mut at = path.clone();
        for _ in 0..=depth {
            at = at.parent().expect("the path is above its root");
        }
        assert_eq!(at.as_str(), *expected);
    }
    let mut at_root = path;
    for _ in ancestors {
        at_root = at_root.parent().expect("the path is above its root");
    }
    assert!(
        at_root.parent().is_none(),
        "the last ancestor is the root and has no parent"
    );
}

#[test]
fn parent_of_a_unicode_unc_path_keeps_the_share() {
    let path = TargetPath::new(windows_target(), r"\\sérveur\partagé\toolé").unwrap();
    assert_eq!(path.parent().unwrap().as_str(), r"\\sérveur\partagé");
    assert!(path.parent().unwrap().parent().is_none());
    assert_eq!(path.file_name(), Some("toolé"));
}

#[rstest]
#[case::windows_drive_root(r"C:\", "windows")]
#[case::windows_unc_root(r"\\server\share", "windows")]
#[case::windows_unicode_unc_root(r"\\sérveur\partagé", "windows")]
#[case::unix_root("/", "unix")]
fn roots_have_no_parent_or_file_name(#[case] path: &str, #[case] target_kind: &str) {
    let target = match target_kind {
        "unix" => unix_target(),
        _ => windows_target(),
    };
    let root = TargetPath::new(target, path).unwrap();
    assert!(root.parent().is_none(), "path: {path:?}");
    assert_eq!(root.file_name(), None, "path: {path:?}");
}

#[rstest]
#[case::empty("", "windows")]
#[case::relative("foo/bar", "windows")]
#[case::traversal(r"C:\PF\..\Windows", "windows")]
#[case::dot(r"C:\PF\.\Windows", "windows")]
#[case::nul("C:\\PF\\Windows\0", "windows")]
#[case::unresolved(r"C:\PF\${install}", "windows")]
#[case::windows_on_unix(r"C:\Windows", "unix")]
#[case::windows_device(r"\\?\C:\Windows", "windows")]
#[case::windows_dot_device(r"\\.\PIPE\device", "windows")]
#[case::unix_on_windows(r"/opt/acme", "windows")]
fn rejects_invalid(#[case] path: &str, #[case] target_kind: &str) {
    let target = match target_kind {
        "unix" => unix_target(),
        _ => windows_target(),
    };
    let error = TargetPath::new(target, path).unwrap_err();
    assert!(
        matches!(
            error,
            TargetPathError::NotAbsolute { .. }
                | TargetPathError::Traversal { .. }
                | TargetPathError::UnresolvedVariable { .. }
                | TargetPathError::Nul { .. }
                | TargetPathError::Empty
        ),
        "path: {path:?}, error: {error:?}"
    );
}

#[test]
fn unix_backslashes_remain_lexical_characters() {
    let base = TargetPath::new(unix_target(), "/opt/acme").unwrap();
    let path = TargetPath::new(unix_target(), "/opt/acme\\tool").unwrap();
    assert_eq!(path.as_str(), "/opt/acme\\tool");
    assert!(!path.starts_with(&base));
    assert_eq!(base.join("C:tool").unwrap().as_str(), "/opt/acme/C:tool");
}

#[test]
fn preserves_case_and_normalizes_only_separators() {
    let path = TargetPath::new(windows_target(), r"c:/Users/Alice/AppData/Local").unwrap();
    assert_eq!(path.as_str(), r"c:\Users\Alice\AppData\Local");
}

#[test]
fn joins_with_target_separator_and_preserves_case() {
    let base = TargetPath::new(windows_target(), r"C:\PF").unwrap();
    let path = base.join("MixedCase").unwrap();
    assert_eq!(path.as_str(), r"C:\PF\MixedCase");
    assert_eq!(path.target(), base.target());
    assert!(path.starts_with(&TargetPath::new(windows_target(), r"c:\pf").unwrap()));
}

#[test]
fn resolves_semantic_location_into_target_path() {
    let template = Template::parse("${location.programs}/Acme").unwrap();
    let path = resolve_template_path(
        &template,
        &windows_target(),
        &fake_locations(),
        SelectedScope::Machine,
    )
    .unwrap();
    assert_eq!(path.as_str(), r"C:\PF\Acme");
}

#[test]
fn resolves_absolute_literal_for_target() {
    let template = Template::parse(r"C:\Zup Tests\App").unwrap();
    let path = resolve_template_path(
        &template,
        &windows_target(),
        &fake_locations(),
        SelectedScope::User,
    )
    .unwrap();
    assert_eq!(path.as_str(), r"C:\Zup Tests\App");
}

#[test]
fn resolves_nested_location_and_literal_parts() {
    let template = Template::parse("${location.user_data}/Programs/Acme/bin/tool.exe").unwrap();
    let path = resolve_template_path(
        &template,
        &windows_target(),
        &fake_locations(),
        SelectedScope::User,
    )
    .unwrap();
    assert_eq!(
        path.as_str(),
        r"C:\Users\Test\AppData\Local\Programs\Acme\bin\tool.exe"
    );
}

#[test]
fn scope_aware_locations_are_forwarded_to_resolver() {
    let mut locations = fake_locations();
    locations.paths.insert(
        InstallLocation::Menu,
        r"C:\Users\Public\Start Menu".to_owned(),
    );
    let template = Template::parse("${location.menu}/Acme").unwrap();
    let path = resolve_template_path(
        &template,
        &windows_target(),
        &locations,
        SelectedScope::Machine,
    )
    .unwrap();
    assert_eq!(path.as_str(), r"C:\Users\Public\Start Menu\Acme");
}

#[test]
fn rejects_unresolved_non_location_variable() {
    let template = Template::parse("${app.name}/bin").unwrap();
    let error = resolve_template_path(
        &template,
        &windows_target(),
        &fake_locations(),
        SelectedScope::User,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TemplateResolveError::UnresolvedVariable {
            variable: Variable::AppName
        }
    ));
}

#[test]
fn resolver_answers_for_the_requested_target() {
    let mut locations = fake_locations();
    locations
        .paths
        .insert(InstallLocation::Programs, "/opt/pf".to_owned());
    let template = Template::parse("${location.programs}/Acme").unwrap();

    let path = resolve_template_path(
        &template,
        &unix_target(),
        &locations,
        SelectedScope::Machine,
    )
    .unwrap();

    assert_eq!(path.as_str(), "/opt/pf/Acme");
    assert_eq!(path.target(), &unix_target());
}

#[test]
fn location_that_is_not_absolute_for_the_target_fails_resolution() {
    let mut locations = fake_locations();
    locations
        .paths
        .insert(InstallLocation::Programs, "relative/pf".to_owned());
    let template = Template::parse("${location.programs}/Acme").unwrap();

    let error = resolve_template_path(
        &template,
        &windows_target(),
        &locations,
        SelectedScope::Machine,
    )
    .unwrap_err();

    assert!(
        matches!(
            error,
            TemplateResolveError::InstallLocation(InstallLocationError::ResolutionFailed {
                location: InstallLocation::Programs,
                scope: SelectedScope::Machine,
                ..
            })
        ),
        "error: {error:?}"
    );
}

#[test]
fn resolver_answering_for_two_targets_in_one_template_is_rejected() {
    let template = Template::parse("${location.programs}/${location.menu}").unwrap();

    let error = resolve_template_path(
        &template,
        &windows_target(),
        &DriftingTargetLocations,
        SelectedScope::Machine,
    )
    .unwrap_err();

    assert!(
        matches!(
            error,
            TemplateResolveError::ResolverTargetMismatch { ref path, ref target }
                if path == "/opt/pf" && target == "x86_64-unknown-linux-gnu"
        ),
        "error: {error:?}"
    );
}

#[test]
fn blames_substituted_text_for_an_unresolved_sequence_in_a_literal() {
    // The app name resolves to text that still carries a template sequence, so
    // the sequence is in substituted text rather than in the manifest.
    let template = Template::parse("${location.programs}/${app.name}")
        .unwrap()
        .substitute(|variable| match variable {
            Variable::AppName => Some(VariableValue::Literal("${app.name}".to_owned())),
            _ => None,
        });

    let error = resolve_template_path(
        &template,
        &windows_target(),
        &fake_locations(),
        SelectedScope::Machine,
    )
    .unwrap_err();

    assert!(
        matches!(
            error,
            TemplateResolveError::UnresolvedLiteral { ref text } if text == "/${app.name}"
        ),
        "error: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        "substituted text contains an unresolved `${` sequence: `/${app.name}`"
    );
}
