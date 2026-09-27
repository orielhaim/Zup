//! Finding a real toolchain component a composition test composes into.
//!
//! Some tests write a component's bytes into a real PE, or read a real launcher's
//! size, and a synthetic header is not enough for either. Those tests need the
//! components `cargo xtask toolchain build` produced, which the toolchain resolver
//! would find on its own — but a test that has to name the launcher's size on the
//! command line has to know where it is.
//!
//! So the search is spelled out here, next to the name the resolver uses, and the
//! failure says the one command that fixes it. A test that cannot find a component
//! fails with that message rather than quietly composing nothing.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use zup_toolchain::ToolchainComponent;

/// The zup version whose components the staged toolchain holds.
///
/// The integration tests share the crate's version because a component stamped
/// with any other version is refused by the descriptor, which is the contract
/// being exercised rather than something to work around.
const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The suffix this machine writes executables with.
const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

/// The staged toolchain component a test needs, beside `executable`.
///
/// `executable` is the running test binary. Cargo puts it in `<profile>/deps`, and
/// the stager writes to `<profile>/toolchain/<version>`, so the profile directory
/// is one level up.
pub fn component(executable: &Path, wanted: &ToolchainComponent) -> PathBuf {
    let profile = profile_directory(executable);
    let name = zup_toolchain::file_name(wanted, EXECUTABLE_SUFFIX);
    for root in [
        profile.join("toolchain").join(ZUP_VERSION),
        profile.join("toolchain"),
    ] {
        let path = root.join(&name);
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "no `{name}` in the staged toolchain beside {}.\n\n  \
         The composition tests need real components. Build them once:\n    \
         cargo xtask toolchain build",
        profile.display()
    );
}

/// One launcher image, for the subsystem and network capability asked for.
///
/// A thin artifact's launcher has to be the one that can resolve a release over
/// the network; an offline launcher composed into one would refuse to install
/// itself on a user's machine.
pub fn dispatcher(executable: &Path, subsystem: zup_toolchain::Subsystem, online: bool) -> PathBuf {
    component(
        executable,
        &ToolchainComponent::Dispatcher { subsystem, online },
    )
}

/// The runtime template for the build host, for one frontend.
pub fn host_runtime(executable: &Path, frontend: zup_core::Frontend) -> PathBuf {
    component(
        executable,
        &ToolchainComponent::Runtime {
            target: zup_core::TargetTriple::parse(zup_plugin_contract::HOST_TARGET)
                .expect("the host's own target triple is valid"),
            frontend,
        },
    )
}

/// The `target/<profile>` directory a test executable was built into.
fn profile_directory(executable: &Path) -> PathBuf {
    let mut directory = executable.parent().unwrap_or(executable).to_path_buf();
    if directory.ends_with("deps") {
        directory.pop();
    }
    directory
}
