use zup_core::{InstallLocation, SelectedScope, TargetTriple};
use zup_platform::{InstallLocationError, InstallLocationResolver, TargetPath};

/// Target-canonical Windows folders for build-time validation.
///
/// The runtime resolves these folders live on the installing machine; a build
/// host validating a target plan needs the same *relations* without touching
/// its own machine. Every target this resolver answers for uses one fixed set
/// of roots, so two builders validating one manifest always agree, and the
/// resolved values never reach an installer: the bundle carries templates, and
/// the runtime re-resolves them live.
#[derive(Debug, Default, Clone, Copy)]
pub struct CanonicalWindowsLocations;

impl InstallLocationResolver for CanonicalWindowsLocations {
    fn resolve(
        &self,
        location: InstallLocation,
        scope: SelectedScope,
        target: &TargetTriple,
    ) -> Result<TargetPath, InstallLocationError> {
        let path = match (location, scope) {
            (InstallLocation::Programs, _) => r"C:\Program Files",
            (InstallLocation::SharedData, _) => r"C:\ProgramData",
            (InstallLocation::UserData, _) => r"C:\Users\zup-build\AppData\Local",
            (InstallLocation::Menu, SelectedScope::User) => {
                r"C:\Users\zup-build\AppData\Roaming\Microsoft\Windows\Start Menu"
            }
            (InstallLocation::Menu, SelectedScope::Machine) => {
                r"C:\ProgramData\Microsoft\Windows\Start Menu"
            }
            (InstallLocation::Desktop, SelectedScope::User) => r"C:\Users\zup-build\Desktop",
            (InstallLocation::Desktop, SelectedScope::Machine) => r"C:\Users\Public\Desktop",
        };
        TargetPath::new(target, path).map_err(|source| InstallLocationError::ResolutionFailed {
            location,
            scope,
            source: source.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::TargetTriple;

    fn target() -> TargetTriple {
        TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
    }

    #[test]
    fn every_location_resolves_to_a_distinct_valid_target_path() {
        let resolver = CanonicalWindowsLocations;
        let target = target();
        for scope in [SelectedScope::User, SelectedScope::Machine] {
            let mut seen = std::collections::BTreeMap::new();
            for location in [
                InstallLocation::Programs,
                InstallLocation::UserData,
                InstallLocation::SharedData,
                InstallLocation::Menu,
                InstallLocation::Desktop,
            ] {
                let path = resolver
                    .resolve(location, scope, &target)
                    .unwrap()
                    .to_string();
                assert!(
                    seen.insert(path.clone(), location).is_none(),
                    "{scope:?} {path}"
                );
            }
        }
    }

    #[test]
    fn machine_and_user_roots_differ_where_they_must() {
        let resolver = CanonicalWindowsLocations;
        let target = target();
        let user_menu = resolver
            .resolve(InstallLocation::Menu, SelectedScope::User, &target)
            .unwrap()
            .to_string();
        let machine_menu = resolver
            .resolve(InstallLocation::Menu, SelectedScope::Machine, &target)
            .unwrap()
            .to_string();
        assert_ne!(user_menu, machine_menu);
        let user_data = resolver
            .resolve(InstallLocation::UserData, SelectedScope::User, &target)
            .unwrap()
            .to_string();
        assert!(user_data.starts_with(r"C:\Users\"), "{user_data}");
    }
}
