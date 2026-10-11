use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanRoom {
    pub material: PathBuf,
    pub work: PathBuf,
    pub artifact: PathBuf,
    pub release: PathBuf,
    pub readiness: String,
}

pub fn run(material: &Path, work: &Path) -> Result<CleanRoom, String> {
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
        return Err(format!("`zup doctor` failed:\n{}", doctor.report()));
    }
    if !doctor.stdout.contains("ready:") {
        return Err(format!(
            "`zup doctor` did not report readiness:\n{}",
            doctor.report()
        ));
    }
    let readiness = doctor
        .stdout
        .lines()
        .find(|line| line.contains("ready:"))
        .unwrap_or_default()
        .trim()
        .to_owned();
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

    let image = zup_binary::Executable::read(&artifact).map_err(|error| {
        format!(
            "`{}` is not an executable this build produced: {error}",
            artifact.display()
        )
    })?;
    if image.format() != zup_binary::BinaryFormat::Pe {
        return Err(format!(
            "`{}` is a {image:?} file, and this release builds Windows images",
            artifact.display()
        ));
    }
    let host = zup_binary::BinaryArchitecture::host().ok_or_else(|| {
        format!(
            "`{}` cannot be checked: this build host runs on a machine zup has no target for",
            artifact.display()
        )
    })?;
    if !image.carries(host) {
        return Err(format!(
            "`{}` is a {} image; this build produces {}",
            artifact.display(),
            image.architectures(),
            host
        ));
    }

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

fn verify_components_come_from_the_release(
    zup: &Path,
    work: &Path,
    material: &Path,
) -> Result<(), String> {
    let outcome = capture(zup, work, &["toolchain", "status", "--format", "json"])?;
    if !outcome.success {
        return Err(format!(
            "`zup toolchain status` failed:\n{}",
            outcome.report()
        ));
    }
    let report: serde_json::Value = serde_json::from_str(&outcome.stdout)
        .map_err(|error| format!("`zup toolchain status` printed no report: {error}"))?;
    let components = report["details"]["components"]
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

fn inherited(name: &str) -> bool {
    if name.starts_with("CARGO") || name.starts_with("RUST") {
        return false;
    }
    !matches!(name, "ZUP_TOOLCHAIN" | "ZUP_DRY_RUN")
}

fn environment() -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = std::env::vars()
        .filter(|(name, _)| inherited(name))
        .map(|(name, value)| (name, Some(value)))
        .collect();
    out.push(("ZUP_TOOLCHAIN".to_owned(), None));
    out.push(("CARGO_MANIFEST_DIR".to_owned(), None));
    out.sort();
    out
}

struct Outcome {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn report(&self) -> String {
        let mut report = self.stdout.clone();
        report.push_str(&self.stderr);
        report
    }
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
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn step(program: &Path, work: &Path, arguments: &[&str], name: &str) -> Result<(), String> {
    let outcome = capture(program, work, arguments)?;
    if !outcome.success {
        return Err(format!(
            "`zup {name}` failed in a clean room:\n{}",
            outcome.report()
        ));
    }
    Ok(())
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
        for name in [
            "ZUP_TOOLCHAIN",
            "CARGO_MANIFEST_DIR",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "RUSTFLAGS",
        ] {
            assert!(!inherited(name), "{name} must not reach a clean room");
        }
        for name in ["PATH", "SystemRoot", "TEMP", "USERPROFILE", "APPDATA"] {
            assert!(inherited(name), "{name} is ordinary process context");
        }

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
        let directory = tempfile::tempdir().expect("temp dir");
        let work = directory.path().join("work");
        std::fs::create_dir_all(&work).expect("create");
        let error = run(directory.path(), &work).expect_err("a used work directory");
        assert!(error.contains("already exists"), "{error}");
    }
}
