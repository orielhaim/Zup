//! The dispatcher: a build input the artifact tests require, and the rule that
//! says which machine it must be.
//!
//! The dispatcher is its own package with a deliberately small dependency
//! closure, so `cargo test` never builds it. That is the right trade for a
//! package whose size is a design constraint, and the wrong trade for the
//! composition tests, which have nothing to compose into without one. So it is
//! built as its own step - `cargo xtask toolchain build`, which CI runs before
//! the tests - and a test that needs one says so instead of quietly passing
//! without testing anything.
//!
//! Everything here is host-neutral path arithmetic. Which machine the
//! dispatcher targets, and how a staged dispatcher is named, are answered by
//! `zup-toolchain` - the contract the developer-side resolver enforces - so a
//! test harness and a build can never disagree about which file a component is.

use std::path::{Path, PathBuf};

use zup_toolchain::{Subsystem, ToolchainComponent};

/// The package that produces the dispatcher.
pub const PACKAGE: &str = "zup-dispatch";

/// The target a dispatcher is built for.
///
/// It has to be the narrowest machine any variant can serve, which on Windows is
/// always 32-bit x86: it runs natively on x86, under WOW64 on x64, and under the
/// x86 compatibility layer on arm64. It is also the smallest of the three, which
/// matters for a file that is downloaded before any of it has been needed.
pub const TARGET: &str = zup_toolchain::DISPATCHER_TARGET;

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

/// The name a staged dispatcher is stored under.
///
/// The machine is in the name on purpose. `cargo build` writes `zup-dispatch.exe`
/// for the *host*, so an installed image that kept that name would be silently
/// replaced by a wider one the next time anybody built the workspace - and every
/// composition test would then fail on a machine-width rule that has nothing to
/// do with what it is testing. The name comes from the toolchain contract rather
/// than from being spelled here a second time, because a harness that guesses a
/// name the stager does not write finds nothing and calls it a missing component.
pub fn installed_name(subsystem: Subsystem, online: bool) -> String {
    zup_toolchain::file_name(
        &ToolchainComponent::Dispatcher { subsystem, online },
        std::env::consts::EXE_SUFFIX,
    )
}

/// The staged names a caller's short name could mean.
fn candidates(name: &str) -> Vec<(String, &'static str)> {
    let both = |subsystem: Subsystem| {
        [
            (installed_name(subsystem, false), "the offline launcher"),
            (installed_name(subsystem, true), "the online launcher"),
        ]
    };
    match name {
        GUI => both(Subsystem::Gui).to_vec(),
        CONSOLE => both(Subsystem::Console).to_vec(),
        ONLINE => vec![(installed_name(Subsystem::Gui, true), "the online launcher")],
        ONLINE_CONSOLE => vec![(
            installed_name(Subsystem::Console, true),
            "the online console launcher",
        )],
        // An unknown short name is looked up under its own name, so a caller that
        // already knows the exact file still works.
        other => vec![(
            format!("{other}{}", std::env::consts::EXE_SUFFIX),
            "the launcher",
        )],
    }
}

/// One dispatcher image in `directory`.
///
/// A short name resolves to the offline image first and the online one second, so
/// a caller that asks for "the launcher" and a tree built without the online
/// feature still finds something; a thin artifact then refuses with a clear
/// reason rather than the test failing on a missing file.
pub fn beside(directory: &Path, name: &str) -> Result<PathBuf, Missing> {
    for (file, _) in candidates(name) {
        let candidate = directory.join(file);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Missing {
        name: name.to_owned(),
        directory: directory.display().to_string(),
    })
}

/// The `target/<profile>` directory a test executable was built into.
fn profile_directory(executable: &Path) -> PathBuf {
    let mut directory = executable.parent().unwrap_or(executable).to_path_buf();
    if directory.ends_with("deps") {
        directory.pop();
    }
    directory
}

/// The two places a staged toolchain may be, in the order the resolver prefers
/// them.
///
/// `cargo xtask toolchain build` stages into `<profile>/toolchain/<version>/` -
/// one directory per zup release, so a stale toolchain beside `zup` can never be
/// half-used with a fresh one - and a project that installs the toolchain next to
/// the executable gets the flat layout. Searching only the first is what makes
/// every composition test fail with a missing template the moment the toolchain
/// gains a version directory; searching only the second is what nobody installs.
fn staged_candidates(executable: &Path) -> Vec<PathBuf> {
    let profile = profile_directory(executable);
    let mut out = Vec::new();
    if let Ok(version) = crate::toolchain::version() {
        out.push(
            profile
                .join(crate::toolchain::STAGED_DIRECTORY)
                .join(version),
        );
    }
    out.push(profile);
    out
}

/// One dispatcher image beside a running test executable.
///
/// This is the same search the developer-side resolver performs, for the same
/// reason: a test that finds its component somewhere a build would not is a test
/// that proves nothing about the real path.
pub fn beside_test_executable(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    let mut last = Missing {
        name: name.to_owned(),
        directory: String::new(),
    };
    for directory in staged_candidates(executable) {
        match beside(&directory, name) {
            Ok(found) => return Ok(found),
            Err(missing) => last = missing,
        }
    }
    Err(last)
}

/// The release build of one dispatcher, next to the test executable's profile.
///
/// Size is a release-profile claim. A debug image carries debuginfo and no
/// optimisation, so measuring one would report a number about the compiler's
/// defaults rather than about the design, and it would be wrong by more than an
/// order of magnitude. A caller that wants the number a user downloads has to
/// ask for the release image, and the release toolchain is a separate build
/// because the size measurement is a property of that profile alone.
pub fn released(executable: &Path, name: &str) -> Result<PathBuf, Missing> {
    let profile = profile_directory(executable);
    let Some(target) = profile.parent().map(Path::to_path_buf) else {
        return beside(&profile, name);
    };
    let release = target.join("release");
    let mut candidates = vec![release.clone()];
    if let Ok(version) = crate::toolchain::version() {
        candidates.insert(
            0,
            release
                .join(crate::toolchain::STAGED_DIRECTORY)
                .join(version),
        );
    }
    let mut last = Missing {
        name: name.to_owned(),
        directory: release.display().to_string(),
    };
    for directory in candidates {
        match beside(&directory, name) {
            Ok(found) => return Ok(found),
            Err(missing) => last = missing,
        }
    }
    Err(last)
}
