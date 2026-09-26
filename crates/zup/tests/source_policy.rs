#![cfg(all(feature = "build", windows))]

//! The CLI materializes sources with the Windows source policy, so a
//! prerequisite reached through a directory junction is refused in production
//! and not only behind the injected-policy entry point.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;
use zup_core::hash_reader;

const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;
const PREREQUISITE: &[u8] = b"runtime payload";

/// A directory junction: a reparse point that needs no elevated privilege, so
/// the production path is exercised on a stock host.
struct Junction {
    path: PathBuf,
}

impl Junction {
    fn new(link: &Path, target: &Path) -> Self {
        fs::create_dir_all(target).unwrap();
        let status = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed for {}", link.display());
        Self {
            path: link.to_path_buf(),
        }
    }
}

impl Drop for Junction {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

/// A project whose embedded prerequisite is read through `vendor`, a junction
/// resolving to content of exactly the declared size and digest.
fn junction_project() -> (TempDir, Junction) {
    let digest = hash_reader(PREREQUISITE).unwrap().1;
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::write(project.path().join("dist/app.bin"), b"payload").unwrap();
    let outside = project.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("runtime.exe"), PREREQUISITE).unwrap();
    fs::write(
        project.path().join("zup.toml"),
        format!(
            r#"schema = 1

[app]
id = "com.example.junction"
name = "Junction App"
version = "1.0.0"
main = "app.bin"

[build]

[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/JunctionApp"

[[files]]
source = "**/*"
destination = "${{install}}"

[[prerequisites]]
id = "runtime"
name = "Runtime"
requirement = {{ kind = "runtime", id = "windows.vc.v14" }}
package = {{ type = "embedded", path = "vendor/runtime.exe", sha256 = "{}", size = {} }}
"#,
            digest.to_hex(),
            PREREQUISITE.len()
        ),
    )
    .unwrap();
    let junction = Junction::new(&project.path().join("vendor"), &outside);
    (project, junction)
}

fn zup() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zup"))
}

#[test]
fn check_refuses_a_prerequisite_reached_through_a_junction() {
    let (project, _junction) = junction_project();
    let output = zup()
        .args(["check", "--manifest"])
        .arg(project.path().join("zup.toml"))
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr(&output).contains("prerequisite `runtime` is missing or unsafe"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn doctor_reports_a_junctioned_prerequisite_as_a_payload_failure() {
    let (project, _junction) = junction_project();
    let output = zup()
        .args(["doctor", "--manifest"])
        .arg(project.path().join("zup.toml"))
        .args(["--format", "json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let rows = checks(&output, "default");
    let payload = find(&rows, "source_payload");
    assert_eq!(payload["status"], "fail", "{}", payload["message"]);
    assert!(
        payload["message"]
            .as_str()
            .unwrap()
            .contains("prerequisite `runtime` is missing or unsafe"),
        "{}",
        payload["message"]
    );
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn checks(output: &Output, profile: &str) -> Vec<Value> {
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor stdout is not one JSON report: {error}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    report["targets"]
        .as_array()
        .expect("targets is an array")
        .iter()
        .find(|target| target["profile"] == profile)
        .unwrap_or_else(|| panic!("no report for profile {profile}"))["checks"]
        .as_array()
        .expect("checks is an array")
        .clone()
}

fn find<'a>(rows: &'a [Value], kind: &str) -> &'a Value {
    rows.iter()
        .find(|check| check["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} check in {rows:?}"))
}
