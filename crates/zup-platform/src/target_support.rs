use zup_core::{Frontend, Target, TargetOperatingSystem};

pub const SUPPORTED_LINUX_TARGET: &str = "linux-x64";

pub fn is_linux_target(target: &Target) -> bool {
    target.operating_system() == TargetOperatingSystem::Linux
}

pub fn is_supported_target(target: &Target) -> bool {
    match target.operating_system() {
        TargetOperatingSystem::Linux => target.as_str() == SUPPORTED_LINUX_TARGET,
        TargetOperatingSystem::Windows => true,
        _ => false,
    }
}

pub fn supported_frontends(target: &Target) -> Vec<Frontend> {
    match target.operating_system() {
        TargetOperatingSystem::Linux => vec![Frontend::Console, Frontend::Headless],
        _ => vec![Frontend::Gui, Frontend::Console, Frontend::Headless],
    }
}

pub fn supports_frontend(target: &Target, frontend: Frontend) -> bool {
    supported_frontends(target).contains(&frontend)
}

pub fn supports_preset(target: &Target, frontend: Frontend) -> bool {
    target.operating_system() == TargetOperatingSystem::Windows && frontend == Frontend::Gui
}

pub fn supports_universal_artifact(target: &Target) -> bool {
    target.operating_system() == TargetOperatingSystem::Windows
}

pub fn supports_dispatcher(target: &Target) -> bool {
    target.operating_system() == TargetOperatingSystem::Windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("windows-x64", Frontend::Gui, true)]
    #[case("windows-x64", Frontend::Console, true)]
    #[case("windows-x64", Frontend::Headless, true)]
    #[case("linux-x64", Frontend::Gui, false)]
    #[case("linux-x64", Frontend::Console, true)]
    #[case("linux-x64", Frontend::Headless, true)]
    fn frontend_support_is_target_selected(
        #[case] target: &str,
        #[case] frontend: Frontend,
        #[case] expected: bool,
    ) {
        let target = Target::parse(target).unwrap();
        assert_eq!(supports_frontend(&target, frontend), expected);
    }

    #[rstest]
    #[case("windows-x64", Frontend::Gui, true)]
    #[case("windows-x64", Frontend::Console, false)]
    #[case("linux-x64", Frontend::Console, false)]
    #[case("linux-x64", Frontend::Gui, false)]
    fn preset_support_is_target_selected(
        #[case] target: &str,
        #[case] frontend: Frontend,
        #[case] expected: bool,
    ) {
        let target = Target::parse(target).unwrap();
        assert_eq!(supports_preset(&target, frontend), expected);
    }

    #[test]
    fn only_one_linux_target_is_supported() {
        assert!(is_supported_target(&Target::parse("linux-x64").unwrap()));
        assert!(!is_supported_target(&Target::parse("linux-arm64").unwrap()));
        assert!(is_supported_target(&Target::parse("windows-x64").unwrap()));
        assert!(is_supported_target(
            &Target::parse("windows-arm64").unwrap()
        ));
    }

    #[test]
    fn universal_artifacts_are_windows_only() {
        assert!(supports_universal_artifact(
            &Target::parse("windows-x64").unwrap()
        ));
        assert!(!supports_universal_artifact(
            &Target::parse("linux-x64").unwrap()
        ));
    }
}
