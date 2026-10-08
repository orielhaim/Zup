#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use zup_core::{Frontend, TargetTriple};
use zup_linux::test_support::genuine_template;
use zup_toolchain::ToolchainComponent;

fn linux_target() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
}

fn staged_version(template: &std::path::Path) -> String {
    template
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .expect("a versioned toolchain directory")
        .to_owned()
}

#[test]
fn console_and_headless_templates_resolve_checked() {
    for frontend in [Frontend::Console, Frontend::Headless] {
        let template = genuine_template(frontend.as_str());
        let wanted = ToolchainComponent::Runtime {
            target: linux_target(),
            frontend,
        };
        let descriptor = zup_toolchain::read(&template, &wanted, &staged_version(&template))
            .expect("a staged Linux runtime resolves checked");
        assert_eq!(
            descriptor.frontend.as_deref(),
            Some(frontend.as_str()),
            "the descriptor names the frontend it was staged for"
        );
        assert_eq!(
            descriptor.target,
            linux_target().as_str(),
            "the descriptor names the Linux target"
        );
    }
}

#[test]
fn no_linux_gui_runtime_exists_to_resolve() {
    let components = zup_toolchain::supported_components(&linux_target());
    assert!(
        components.iter().all(|component| !matches!(
            component,
            ToolchainComponent::Runtime {
                frontend: Frontend::Gui,
                ..
            }
        )),
        "the contract names no Linux GUI runtime: {components:?}"
    );
    assert_eq!(
        components.len(),
        2,
        "console and headless, and nothing else: {components:?}"
    );
}
