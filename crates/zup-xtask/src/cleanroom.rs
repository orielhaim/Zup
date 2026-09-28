//! The clean-room acceptance run.
//!
//! Everything else in this repository is tested from *inside* it: `cargo test`
//! runs in a workspace with a populated `target/`, the tests resolve toolchain
//! components from a staged directory, and the CLI is the one `cargo` just built.
//! That is the right way to test code and the wrong way to test a *product*.
//!
//! The question this module answers is the one a developer actually asks:
//!
//! > I downloaded zup. I have no source checkout, no `target/`, and no idea what
//! > `xtask` is. Does `zup build` work?
//!
//! So it does the whole thing from a directory it creates, using a copy of the
//! release material and nothing else:
//!
//! ```text
//! verify the release index
//!   → create an empty project directory, outside the workspace
//!   → zup init, zup check, zup doctor, zup build
//!   → the artifact is a real PE carrying a real artifact index
//!   → nothing in the output names the repository
//! ```
//!
//! The environment is scrubbed rather than inherited. `ZUP_TOOLCHAIN` is the
//! obvious one — it is exactly the variable that could make a broken release look
//! like a working one — but `CARGO_HOME`, `RUSTUP_HOME`, and every `CARGO_*`
//! variable go too, because a `cargo run` that works and a downloaded binary that
//! does not are different problems and this has to be able to tell them apart.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What the run proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanRoom {
    /// The release material the run used, in isolation from the workspace.
    pub material: PathBuf,
    /// The directory the project was created in.
    pub work: PathBuf,
    /// The artifact the build produced.
    pub artifact: PathBuf,
    /// The release description the build wrote beside it.
    pub release: PathBuf,
    /// The `zup doctor` readiness line, so a reader sees what it decided.
    pub readiness: String,
}

/// Run the whole clean-room sequence.
pub fn run(material: &Path, work: &Path) -> Result<CleanRoom, String> {
    // The work directory first, because a run in a directory that already has
    // something in it is not a clean room whatever the material turns out to be,
    // and a material error reported on top of that would be the less important
    // one.
    if work.exists() {
        return Err(format!(
            "`{}` already exists; the clean room has to start empty or it is not one",
            work.display()
        ));
    }
    let zup = material.join(executable("zup"));
    if !zup.is_file() {
        return Err(format!(
            "`{}` has no zup executable; run `cargo xtask toolchain package --out {}` first",
            material.display(),
            material.display()
        ));
    }
    let index = zup_toolchain::ToolchainRelease::read(material)
        .map_err(|error| format!("the release material is incomplete: {error}"))?;
    index
        .verify(material)
        .map_err(|error| format!("the release material is not intact: {error}"))?;

    std::fs::create_dir_all(work).map_err(|error| format!("{}: {error}", work.display()))?;

    step(
        &zup,
        work,
        &[
            "init",
            "--name",
            "Acme",
            "--app-id",
            "com.acme.desktop",
            "--non-interactive",
        ],
        "init",
    )?;
    step(&zup, work, &["check"], "check")?;
    let doctor = capture(&zup, work, &["doctor"])?;
    if !doctor.success {
        return Err(format!("`zup doctor` failed:\n{}", doctor.output));
    }
    if !doctor.output.contains("ready:") {
        return Err(format!(
            "`zup doctor` did not report readiness:\n{}",
            doctor.output
        ));
    }
    let readiness = doctor
        .output
        .lines()
        .find(|line| line.contains("ready:"))
        .unwrap_or_default()
        .trim()
        .to_owned();
    // Before the build, so a resolver that has learned to look somewhere else
    // is caught by a report rather than by an artifact nobody expected.
    verify_components_come_from_the_release(&zup, work, material)?;
    step(&zup, work, &["build"], "build")?;

    let artifact = work.join("Acme-Setup.exe");
    if !artifact.is_file() {
        return Err(format!(
            "`zup build` reported success but wrote no `{}`",
            artifact.display()
        ));
    }
    let release = work.join(zup_artifact::RELEASE_MANIFEST_NAME);
    if !release.is_file() {
        return Err(format!(
            "`zup build` wrote no release description at `{}`",
            release.display()
        ));
    }

    // The artifact is a real image, not a file that happens to exist. The DOS
    // stub, the `e_lfanew` pointer, and the PE signature are the three facts a
    // downloader can check without executing anything, and reading the signature
    // *through* the pointer rather than at a fixed offset is what makes this a
    // check rather than a coincidence about linkers.
    let head = read_head(&artifact, 0x100)?;
    if &head[..2] != b"MZ" {
        return Err(format!(
            "`{}` does not start with a DOS header",
            artifact.display()
        ));
    }
    let pe = u32::from_le_bytes(head[0x3c..0x40].try_into().expect("four bytes")) as usize;
    if pe + 6 > head.len() || &head[pe..pe + 4] != b"PE\0\0" {
        return Err(format!(
            "`{}` has no PE header at the offset its DOS header names",
            artifact.display()
        ));
    }
    // And the machine it is for, which is the fact a dispatcher selects on. A
    // build that produced an image for the wrong architecture would still be a
    // valid PE and would still install on a machine that cannot run it.
    let machine = u16::from_le_bytes(head[pe + 4..pe + 6].try_into().expect("two bytes"));
    let expected = if cfg!(target_arch = "aarch64") {
        0xaa64
    } else {
        0x8664
    };
    if machine != expected {
        return Err(format!(
            "`{}` is machine 0x{machine:04x}; this build produces 0x{expected:04x}",
            artifact.display()
        ));
    }

    // The one thing that must never survive into a release: the build machine's
    // own directory. A description naming it is a description of a build, not of
    // a release, and no downloader can use it.
    let text = std::fs::read_to_string(&release)
        .map_err(|error| format!("{}: {error}", release.display()))?;
    let repository = repository_root();
    if let Some(name) = repository.file_name()
        && text.contains(&name.to_string_lossy().to_string())
    {
        return Err(format!(
            "`{}` names `{}`, so the build recorded where it ran",
            release.display(),
            name.to_string_lossy()
        ));
    }
    for needle in ["C:\\", "target\\", "target/", ".cargo", "registry/src"] {
        if text.contains(needle) {
            return Err(format!(
                "`{}` contains `{needle}`, which is a build-machine path",
                release.display()
            ));
        }
    }

    Ok(CleanRoom {
        material: material.to_path_buf(),
        work: work.to_path_buf(),
        artifact,
        release,
        readiness,
    })
}

/// Where every component a build composes from has to come from.
///
/// A downloaded `zup` resolves its runtime templates and launchers out of the
/// directory it was unpacked into. That is a strong claim and the one most
/// likely to rot: a resolver that gained an ambient arm — a `%PATH%` entry, a
/// component sitting beside the project, a machine-global directory — would
/// still work in this repository, where `target/` is full of real components,
/// and would compose a different installer on a user's machine.
///
/// So it is asked directly. Every component must resolve, every one must say
/// `staged`, and every path must be inside the versioned toolchain directory of
/// the release this run was handed. The material itself usually lives under
/// `target/` in the checkout that built it, so "not inside the repository" would
/// be a false alarm rather than a guarantee; being inside *this* directory is
/// the guarantee, because the staged toolchain beside the test binary is a
/// different path with the same file names.
fn verify_components_come_from_the_release(
    zup: &Path,
    work: &Path,
    material: &Path,
) -> Result<(), String> {
    let outcome = capture(zup, work, &["toolchain", "status", "--format", "json"])?;
    if !outcome.success {
        return Err(format!(
            "`zup toolchain status` failed:\n{}",
            outcome.output
        ));
    }
    let report: serde_json::Value = serde_json::from_str(&outcome.output)
        .map_err(|error| format!("`zup toolchain status` printed no report: {error}"))?;
    let components = report["components"]
        .as_array()
        .ok_or("`zup toolchain status` reported no components")?;
    if components.is_empty() {
        return Err("`zup toolchain status` reported no components at all".to_owned());
    }
    let expected = plain(material).join(crate::toolchain::STAGED_DIRECTORY);
    for component in components {
        let name = component["component"]
            .as_str()
            .unwrap_or("<unnamed>")
            .to_owned();
        if !component["found"].as_bool().unwrap_or(false) {
            return Err(format!(
                "no {name} resolved; the release material is incomplete"
            ));
        }
        match component["source"].as_str() {
            Some("staged") => {}
            Some(other) => {
                return Err(format!(
                    "{name} came from the {other} arm, so this release is not self-contained: {}",
                    component["path"].as_str().unwrap_or("<no path>")
                ));
            }
            None => return Err(format!("{name} resolved without saying from where")),
        }
        let Some(path) = component["path"].as_str() else {
            return Err(format!("{name} resolved without a path"));
        };
        // The versioned directory, not just the release root: that is the one
        // the resolver searches, and matching on it is what proves it used the
        // layout rather than finding a file that happened to be nearby.
        if !Path::new(path).starts_with(&expected) {
            return Err(format!(
                "{name} resolved to `{path}`, which is not inside the release's toolchain \
                 directory at {}",
                expected.display()
            ));
        }
    }
    Ok(())
}

/// A path without the Windows verbatim prefix.
///
/// `canonicalize` answers with `\\?\…` on Windows, and two spellings of one
/// directory do not compare equal. This crate is portable, so it cannot ask the
/// Windows crate to strip the prefix; it strips it here instead, and only for
/// this comparison.
fn plain(path: &Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    let stripped = text
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned));
    match stripped {
        Some(text) => std::path::PathBuf::from(text),
        None => path.to_path_buf(),
    }
}

/// Whether a variable from this process may reach a clean-room subprocess.
///
/// A predicate rather than a literal list so the test can ask the same question
/// the function does, without the test having to mutate the process's own
/// environment to do it — which would be an `unsafe` block in a crate that
/// forbids one, and a data race with every other test in the same binary.
fn inherited(name: &str) -> bool {
    if name.starts_with("CARGO") || name.starts_with("RUST") {
        return false;
    }
    !matches!(name, "ZUP_TOOLCHAIN" | "ZUP_DRY_RUN")
}

/// The environment a clean-room subprocess sees.
///
/// A scrub rather than an extension, because an inherited `ZUP_TOOLCHAIN` or a
/// `CARGO_MANIFEST_DIR` from the parent is exactly how a run that passed inside
/// this repository fails outside it.
fn environment() -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = std::env::vars()
        .filter(|(name, _)| inherited(name))
        .map(|(name, value)| (name, Some(value)))
        .collect();
    // Present-but-absent, not merely scrubbed: an empty value and no value are
    // different things to a program that reads one, and this needs the
    // unambiguous one.
    out.push(("ZUP_TOOLCHAIN".to_owned(), None));
    out.push(("CARGO_MANIFEST_DIR".to_owned(), None));
    out.sort();
    out
}

struct Outcome {
    success: bool,
    output: String,
}

fn capture(program: &Path, work: &Path, arguments: &[&str]) -> Result<Outcome, String> {
    let mut command = Command::new(program);
    command.current_dir(work).args(arguments);
    command.env_clear();
    for (name, value) in environment() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("run `{}`: {error}", program.display()))?;
    Ok(Outcome {
        success: output.status.success(),
        output: String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr),
    })
}

fn step(program: &Path, work: &Path, arguments: &[&str], name: &str) -> Result<(), String> {
    let outcome = capture(program, work, arguments)?;
    if !outcome.success {
        return Err(format!(
            "`zup {name}` failed in a clean room:\n{}",
            outcome.output
        ));
    }
    Ok(())
}

fn read_head(path: &Path, length: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut buffer = vec![0u8; length];
    file.read_exact(&mut buffer)
        .map_err(|error| format!("{} is shorter than a PE header: {error}", path.display()))?;
    Ok(buffer)
}

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn repository_root() -> PathBuf {
    crate::toolchain::repository_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_room_never_inherits_the_variables_that_would_fake_one() {
        // The variables a build could discover this repository through, and the
        // reason each is absent. A clean room that inherits one of these is a
        // clean room that proves nothing.
        for name in [
            "ZUP_TOOLCHAIN",
            "CARGO_MANIFEST_DIR",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "RUSTFLAGS",
        ] {
            assert!(!inherited(name), "{name} must not reach a clean room");
        }
        // What a downloaded binary legitimately needs is still there.
        for name in ["PATH", "SystemRoot", "TEMP", "USERPROFILE", "APPDATA"] {
            assert!(inherited(name), "{name} is ordinary process context");
        }

        // And the two that must be pinned are pinned *absent* rather than scrubbed,
        // because an empty value reads differently from no value.
        let environment = environment();
        for name in ["ZUP_TOOLCHAIN", "CARGO_MANIFEST_DIR"] {
            let entry = environment
                .iter()
                .find(|(candidate, _)| candidate == name)
                .unwrap_or_else(|| panic!("{name} is not pinned"));
            assert!(entry.1.is_none(), "{name} must be absent, not empty");
        }
    }

    #[test]
    fn a_clean_room_refuses_a_material_directory_with_no_index() {
        let directory = tempfile::tempdir().expect("temp dir");
        let work = directory.path().join("work");
        let error = run(directory.path(), &work).expect_err("an empty directory is not a release");
        assert!(error.contains("zup"), "{error}");
    }

    #[test]
    fn a_clean_room_refuses_a_work_directory_that_already_has_things_in_it() {
        // Otherwise a run could pass by finding a project somebody left behind,
        // which is the opposite of what the test is for.
        let directory = tempfile::tempdir().expect("temp dir");
        let work = directory.path().join("work");
        std::fs::create_dir_all(&work).expect("create");
        let error = run(directory.path(), &work).expect_err("a used work directory");
        assert!(error.contains("already exists"), "{error}");
    }
}
