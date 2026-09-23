//! Target path and template resolution unit tests (cross-platform).

use std::collections::BTreeMap;
use std::path::PathBuf;

use rstest::rstest;
use zup_core::{SelectedScope, Template, Variable};
use zup_platform::{
    KnownFolder, KnownFolderError, KnownFolderResolver, TargetPath, TargetPathError,
    TemplateResolveError, resolve_template_path,
};

/// Deterministic fake known-folder resolver for tests.
#[derive(Debug, Default, Clone)]
struct FakeKnownFolders {
    paths: BTreeMap<KnownFolder, PathBuf>,
}

impl KnownFolderResolver for FakeKnownFolders {
    fn resolve(
        &self,
        folder: KnownFolder,
        _scope: SelectedScope,
    ) -> Result<PathBuf, KnownFolderError> {
        self.paths
            .get(&folder)
            .cloned()
            .ok_or_else(|| KnownFolderError::ResolutionFailed {
                folder,
                scope: SelectedScope::User,
                source: "not configured".into(),
            })
    }
}

fn fake_resolver() -> FakeKnownFolders {
    let mut paths = BTreeMap::new();
    paths.insert(KnownFolder::ProgramFiles, PathBuf::from(r"C:\PF"));
    paths.insert(
        KnownFolder::LocalAppData,
        PathBuf::from(r"C:\Users\Test\AppData\Local"),
    );
    paths.insert(KnownFolder::ProgramData, PathBuf::from(r"C:\PD"));
    paths.insert(KnownFolder::StartMenu, PathBuf::from(r"C:\SM"));
    paths.insert(KnownFolder::Desktop, PathBuf::from(r"C:\DESK"));
    FakeKnownFolders { paths }
}

#[rstest]
#[case::simple(r"C:\Windows\System32\cmd.exe")]
fn accepts_absolute(#[case] path: &str) {
    assert!(TargetPath::new(PathBuf::from(path)).is_ok());
}

#[rstest]
#[case::relative("foo/bar")]
#[case::traversal_dot_dot(r"C:\PF\..\Windows")]
#[case::unresolved(r"C:\PF\${install}")]
#[case::empty("")]
fn rejects_invalid(#[case] path: &str) {
    let err = TargetPath::new(PathBuf::from(path)).unwrap_err();
    assert!(
        matches!(
            err,
            TargetPathError::NotAbsolute { .. }
                | TargetPathError::Traversal { .. }
                | TargetPathError::UnresolvedVariable { .. }
                | TargetPathError::Empty
        ),
        "path: {path}, err: {err:?}"
    );
}

#[test]
fn middle_dot_is_normalized_by_pathbuf() {
    // `Path::components` drops interior `.` segments; the resulting absolute
    // path is still valid and contains no traversal.
    let path = TargetPath::new(PathBuf::from(r"C:\PF\.\x")).unwrap();
    assert!(!path.as_path().components().any(|c| matches!(
        c,
        std::path::Component::CurDir | std::path::Component::ParentDir
    )));
}

#[test]
fn install_directory_resolved() {
    let template = Template::parse("${known.program_files}/Acme").unwrap();
    let path = resolve_template_path(&template, &fake_resolver(), SelectedScope::Machine).unwrap();
    assert_eq!(path.to_string(), r"C:\PF\Acme");
}

#[test]
fn absolute_literal_template_keeps_windows_root() {
    let template = Template::parse(r"C:\Zup Tests\App").unwrap();
    let path = resolve_template_path(&template, &fake_resolver(), SelectedScope::User).unwrap();
    assert_eq!(path.to_string(), "C:/Zup Tests/App");
}

#[test]
fn nested_destination_resolved() {
    let template =
        Template::parse("${known.local_app_data}/Programs/${app.name}/bin/tool.exe").unwrap();
    // app.name should have been substituted already in InstallPlan; after
    // resolve_template it becomes a literal. Here we only resolve known.*.
    // So substitute first like the planner does.
    let install = Template::parse("${known.program_files}/Acme").unwrap();
    let planned = template.substitute(|var| match var {
        Variable::AppName => Some(zup_core::VariableValue::Literal("Acme".into())),
        Variable::Install => Some(zup_core::VariableValue::Template(install.clone())),
        _ => None,
    });
    // Direct known-folder template
    let path = resolve_template_path(&planned, &fake_resolver(), SelectedScope::User).unwrap();
    assert_eq!(
        path.to_string(),
        r"C:\Users\Test\AppData\Local\Programs\Acme\bin\tool.exe"
    );
}

#[test]
fn user_vs_machine_shell_folders() {
    let mut user = fake_resolver();
    user.paths.insert(
        KnownFolder::StartMenu,
        PathBuf::from(r"C:\Users\Test\Start Menu"),
    );
    user.paths.insert(
        KnownFolder::Desktop,
        PathBuf::from(r"C:\Users\Test\Desktop"),
    );

    let mut machine = fake_resolver();
    machine.paths.insert(
        KnownFolder::StartMenu,
        PathBuf::from(r"C:\ProgramData\Microsoft\Windows\Start Menu"),
    );
    machine.paths.insert(
        KnownFolder::Desktop,
        PathBuf::from(r"C:\Users\Public\Desktop"),
    );

    let template = Template::parse("${known.start_menu}/Acme").unwrap();
    let user_path = resolve_template_path(&template, &user, SelectedScope::User).unwrap();
    let machine_path = resolve_template_path(&template, &machine, SelectedScope::Machine).unwrap();
    assert_eq!(user_path.to_string(), r"C:\Users\Test\Start Menu\Acme");
    assert_eq!(
        machine_path.to_string(),
        r"C:\ProgramData\Microsoft\Windows\Start Menu\Acme"
    );
}

#[test]
fn rejects_relative_result() {
    let mut fake = FakeKnownFolders::default();
    fake.paths
        .insert(KnownFolder::ProgramFiles, PathBuf::from("relative/pf"));
    let template = Template::parse("${known.program_files}/Acme").unwrap();
    let err = resolve_template_path(&template, &fake, SelectedScope::User).unwrap_err();
    assert!(matches!(
        err,
        TemplateResolveError::InvalidPath(TargetPathError::NotAbsolute { .. })
    ));
}

#[test]
fn rejects_unresolved_non_known_variable() {
    // After static substitution, `${app.name}` should not remain. If it does,
    // resolution must fail.
    let template = Template::parse("${app.name}/bin").unwrap();
    let err = resolve_template_path(&template, &fake_resolver(), SelectedScope::User).unwrap_err();
    assert!(matches!(
        err,
        TemplateResolveError::UnresolvedVariable {
            variable: Variable::AppName
        }
    ));
}
