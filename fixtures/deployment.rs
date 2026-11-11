pub fn requirements(
    target: zup_core::Target,
    scope: zup_core::SelectedScope,
    app_id: &str,
    version: &str,
) -> zup_plan::DeploymentRequirements {
    zup_plan::DeploymentRequirements {
 generation_readers: Default::default(),
        data: vec![],
        app: zup_core::App {
            id: zup_core::AppId::new(app_id).unwrap(),
            name: zup_core::NonEmptyString::new("Fixture").unwrap(),
            version: version.parse().unwrap(), publisher: None, main: None, description: None,
        },
        target, scope,
        directory: zup_core::Template::parse(match target.operating_system() {
            zup_core::TargetOperatingSystem::Windows => r"C:\application",
            zup_core::TargetOperatingSystem::Linux => "/application",
            _ => panic!("unsupported fixture target"),
        }).unwrap(),
        components: vec![], prerequisites: vec![], files: vec![], launchers: vec![],
        path_entries: vec![], services: vec![], protocols: vec![], file_associations: vec![], preset: None,
    }
}
