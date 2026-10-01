use std::collections::BTreeMap;

use rstest::rstest;
use zup_core::{InstallLocation, SelectedScope, TargetTriple, Template, Variable, VariableValue};
use zup_platform::{
    InstallLocationError, InstallLocationResolver, TargetPath, TemplateResolveError,
    resolve_template_path,
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

#[rstest]
#[case::parent(r"${location.programs}/../Windows")]
#[case::nested_parent(r"${location.programs}/bin/..")]
#[case::drive(r"${location.programs}/D:/Windows")]
#[case::drive_relative(r"${location.programs}/C:Windows")]
fn a_literal_relocating_the_path_is_refused(#[case] template: &str) {
    let error = resolve_template_path(
        &Template::parse(template).unwrap(),
        &windows_target(),
        &fake_locations(),
        SelectedScope::Machine,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            TemplateResolveError::InvalidSegment { .. } | TemplateResolveError::InvalidPath(_)
        ),
        "{template}: {error:?}"
    );
}

#[test]
fn a_literal_is_joined_onto_the_resolved_location() {
    let path = resolve_template_path(
        &Template::parse("${location.programs}/Programs/Acme/bin/tool.exe").unwrap(),
        &windows_target(),
        &fake_locations(),
        SelectedScope::Machine,
    )
    .unwrap();
    assert_eq!(path.as_str(), r"C:\PF\Programs\Acme\bin\tool.exe");
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
