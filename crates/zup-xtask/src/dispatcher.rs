//! The dispatcher: a build input the artifact tests require, and the rule that
//! says which machine it must be.
//!
//! The dispatcher is its own package with a deliberately small dependency
//! closure, so `cargo test` never builds it. That is the right trade for a
//! package whose size is a design constraint, and the wrong trade for the
//! composition tests, which have nothing to compose into without one. So it is
//! built as its own step — `scripts/build-dispatcher.ps1`, which CI runs before
//! the tests — and a test that needs one says so instead of quietly passing
//! without testing anything.
//!
//! Everything here is host-neutral path arithmetic. Which machine the
//! dispatcher targets is a Windows decision, and it belongs to the crate that
//! makes it.

use std::path::{Path, PathBuf};

/// The package that produces the dispatcher.
pub const PACKAGE: &str = "zup-dispatch";

/// The target a dispatcher is built for.
///
/// It has to be the narrowest machine any variant can serve, which on Windows is
/// always 32-bit x86: it runs natively on x86, under WOW64 on x64, and under the
/// x86 compatibility layer on arm64. It is also the smallest of the three, which
/// matters for a file that is downloaded before any of it has been needed.
pub const TARGET: &str = "i686-pc-windows-msvc";

/// The two dispatcher images: a windowed launcher and a console one.
pub const GUI: &str = "zup-dispatch";
pub const CONSOLE: &str = "zup-dispatch-console";

/// The windowed dispatcher built with the online path.
///
/// It is a separate image, not a flag, because the whole point of the measurement
/// is to state what the online path costs: a launcher with a TUF client, an HTTP
/// transport, and an acquisition engine is a different file from one without
/// them, and the difference is the number a bootstrapper's size is made of.
pub const ONLINE: &str = "zup-dispatch-online";

/// The console dispatcher built with the online path.
pub const ONLINE_CONSOLE: &str = "zup-dispatch-console-online";

/// Why a dispatcher template is not where it should be.
#[derive(Debug, thiserror::Error)]
#[error(
    "the {name} dispatcher is not in {directory}; run `cargo xtask toolchain build` and try again"
)]
pub struct Missing {
    pub name: String,
    pub directory: String,
}

/// The name an installed dispatcher carries.
///
/// The machine is in the name on purpose. `cargo build` writes
/// `zup-dispatch.exe` for the *host*, so an installed image that kept that name
/// would be silently replaced by a wider one the next time anybody built the
/// workspace — and every composition test would then fail on a machine-width rule
/// that has nothing to do with what it is testing. Naming the image for the
/// machine it is makes the two impossible to confuse.
pub fn installed_name(name: &str) -> String {
    format!("{name}-{TARGET}{}", std::env::consts::EXE_SUFFIX)
}

/// One dispatcher image in `directory`.
///
/// The online image is preferred when a caller asks for an online one, and the
/// search falls back to the offline image so a tree built without the online
/// feature still works — a thin artifact then refuses with a clear reason rather
/// than the test failing on a missing file.
pub fn beside(directory: &Path, name: &str) -> Result<PathBuf, Missing> {
    for candidate in [
        directory.join(installed_name(name)),
        directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
    ] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Missing {
        name: name.to_owned(),
        directory: directory.display().to_string(),
    })
}

/// The directory beside a running test executable, which is where a build step
/// installs the images the tests compose into.
pub fn beside_test_executable(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    beside(&profile_directory(executable), name)
}

/// The `target/<profile>` directory a test executable was built into.
fn profile_directory(executable: &Path) -> PathBuf {
    let mut directory = executable.parent().unwrap_or(executable).to_path_buf();
    if directory.ends_with("deps") {
        directory.pop();
    }
    directory
}

/// The release build of one dispatcher, next to the test executable's profile.
///
/// Size is a release-profile claim. A debug image carries debuginfo and no
/// optimisation, so measuring one would report a number about the compiler's
/// defaults rather than about the design, and it would be wrong by more than an
/// order of magnitude. A caller that wants the number a user downloads has to
/// ask for the release image.
pub fn released(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    let profile = profile_directory(executable);
    let release = profile
        .parent()
        .map(|target| target.join("release"))
        .unwrap_or_else(|| profile.clone());
    beside(&release, name)
}
