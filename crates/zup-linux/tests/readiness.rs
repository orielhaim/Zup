#![cfg(target_os = "linux")]

//! Linux toolchain readiness: what the build can resolve, and what it cannot.
//!
//! `zup doctor` is Windows-only in this phase, so this is where Linux answers
//! the readiness question instead: the staged toolchain resolves checked
//! console and headless runtimes for `x86_64-unknown-linux-gnu` - descriptor,
//! version, target, frontend, and digest all verified - and there is no GUI
//! component to resolve. The absence is structural (the contract names no
//! Linux GUI runtime) rather than a missing file, so no fallback can silently
//! substitute a console template where a window was asked for.

#[path = "support.rs"]
mod support;

use zup_core::{Frontend, TargetTriple};
use zup_toolchain::ToolchainComponent;

fn linux_target() -> TargetTriple {
    TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target")
}

/// The staged version, read off the toolchain directory rather than spelled
/// out: the test asserts about the toolchain that is actually staged, not
/// about a version string that drifted from it.
fn staged_version(template: &std::path::Path) -> String {
    template
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .expect("a versioned toolchain directory")
        .to_owned()
}

/// Console and headless resolve to checked components.
///
/// Descriptor, zup version, target, frontend, and content digest are all
/// verified by the read - the same check a build performs before composing -
/// so a stale or foreign binary cannot slip in under a right-looking name.
#[test]
fn console_and_headless_templates_resolve_checked() {
    for frontend in [Frontend::Console, Frontend::Headless] {
        let template = support::genuine_template(frontend.as_str());
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

/// There is no Linux GUI runtime to resolve - not a missing file, but no such
/// component in the contract. A build that asked for one must fail naming the
/// absence, never fall back to a console template and report a window.
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
