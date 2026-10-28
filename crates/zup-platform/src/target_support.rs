use zup_core::{Frontend, SelectedScope, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSupport {
    Unsupported,
    User,
    Machine,
    Both,
}

impl ScopeSupport {
    pub fn supports(self, scope: SelectedScope) -> bool {
        matches!(
            (self, scope),
            (Self::Both, _)
                | (Self::User, SelectedScope::User)
                | (Self::Machine, SelectedScope::Machine)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactSupport {
    Native,
    Universal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    Uac,
    Polkit,
}

#[derive(Debug)]
pub struct TargetCapabilities {
    pub frontends: &'static [Frontend],
    pub scopes: ScopeSupport,
    pub services: ScopeSupport,
    pub menu: ScopeSupport,
    pub desktop: ScopeSupport,
    pub protocols: ScopeSupport,
    pub file_associations: ScopeSupport,
    pub search_path: ScopeSupport,
    pub prerequisites: ScopeSupport,
    pub plugins: ScopeSupport,
    pub artifacts: ArtifactSupport,
    pub elevation: Elevation,
}

impl TargetCapabilities {
    pub fn resource_errors(
        &self,
        installer: &zup_core::Installer,
        scope: SelectedScope,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.scopes.supports(scope) {
            errors.push(format!(
                "target `{}` does not support {scope} installations",
                installer.target
            ));
        }
        if !self.frontends.contains(&installer.frontend) {
            errors.push(format!(
                "target `{}` does not support the {} frontend",
                installer.target, installer.frontend
            ));
        }
        let menu = installer
            .launchers
            .iter()
            .filter(|launcher| launcher.location == zup_core::LauncherLocation::Menu)
            .count();
        let desktop = installer.launchers.len() - menu;
        for (kind, count, support) in [
            ("service", installer.services.len(), self.services),
            ("menu launcher", menu, self.menu),
            ("desktop launcher", desktop, self.desktop),
            ("URI protocol", installer.protocols.len(), self.protocols),
            (
                "file association",
                installer.file_associations.len(),
                self.file_associations,
            ),
            ("PATH entry", installer.path.len(), self.search_path),
            (
                "package-manager prerequisite",
                installer.prerequisites.len(),
                self.prerequisites,
            ),
            ("plugin", installer.plugins.len(), self.plugins),
        ] {
            if count > 0 && !support.supports(scope) {
                errors.push(format!(
                    "target `{}` does not support {kind} resources in {scope} scope",
                    installer.target
                ));
            }
        }
        errors
    }

    pub fn supports_preset(&self, frontend: Frontend) -> bool {
        frontend == Frontend::Gui && self.frontends.contains(&frontend)
    }
}

const FRONTENDS: &[Frontend] = &[Frontend::Gui, Frontend::Console, Frontend::Headless];
const WINDOWS: TargetCapabilities = TargetCapabilities {
    frontends: FRONTENDS,
    scopes: ScopeSupport::Both,
    services: ScopeSupport::Machine,
    menu: ScopeSupport::Both,
    desktop: ScopeSupport::Both,
    protocols: ScopeSupport::Both,
    file_associations: ScopeSupport::Both,
    search_path: ScopeSupport::Both,
    prerequisites: ScopeSupport::Both,
    plugins: ScopeSupport::Both,
    artifacts: ArtifactSupport::Universal,
    elevation: Elevation::Uac,
};
const LINUX: TargetCapabilities = TargetCapabilities {
    frontends: FRONTENDS,
    scopes: ScopeSupport::Both,
    services: ScopeSupport::Machine,
    menu: ScopeSupport::User,
    desktop: ScopeSupport::Unsupported,
    protocols: ScopeSupport::User,
    file_associations: ScopeSupport::User,
    search_path: ScopeSupport::Unsupported,
    prerequisites: ScopeSupport::Unsupported,
    plugins: ScopeSupport::User,
    artifacts: ArtifactSupport::Native,
    elevation: Elevation::Polkit,
};

pub fn capabilities(target: &Target) -> Option<&'static TargetCapabilities> {
    match *target {
        Target::WINDOWS_X64 | Target::WINDOWS_X86 | Target::WINDOWS_ARM64 => Some(&WINDOWS),
        Target::LINUX_X64 | Target::LINUX_ARM64 => Some(&LINUX),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("linux-x64", Some(ArtifactSupport::Native))]
    #[case("linux-arm64", Some(ArtifactSupport::Native))]
    #[case("windows-arm64", Some(ArtifactSupport::Universal))]
    #[case("linux-x64-musl", None)]
    fn target_support_is_explicit(#[case] target: &str, #[case] expected: Option<ArtifactSupport>) {
        assert_eq!(
            capabilities(&Target::parse(target).unwrap()).map(|caps| caps.artifacts),
            expected
        );
    }
}
