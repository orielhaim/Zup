use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};

#[path = "support/toolchain_fixture.rs"]
mod toolchain_fixture;

const PLUGIN_WIT: &str = zup_plugin_abi::WIT_PACKAGE;
const HOST_TARGET: &str = zup_plugin_contract::HOST_TARGET;
#[cfg(windows)]
const LINUX_TARGET: &str = "aarch64-unknown-linux-gnu";

/// A target no backend implements on this build host.
#[cfg(windows)]
const UNSUPPORTED_TARGET: &str = LINUX_TARGET;
#[cfg(not(windows))]
const UNSUPPORTED_TARGET: &str = "x86_64-pc-windows-msvc";

#[cfg(target_arch = "aarch64")]
const OTHER_TARGET: &str = "x86_64-pc-windows-msvc";
#[cfg(not(target_arch = "aarch64"))]
const OTHER_TARGET: &str = "aarch64-pc-windows-msvc";

/// PE templates, Windows lowering, and the Windows backend only exist on Windows.
const ON_WINDOWS: bool = cfg!(windows);

/// Every check kind a report must contain for each selected target.
const CHECK_KINDS: [&str; 10] = [
    "canonical_target",
    "manifest_compile",
    "source_payload",
    "plugin_engine",
    "target_lowering",
    "update_root",
    "frontend",
    "runtime_template",
    "build_backend",
    "output_parent",
];

fn zup() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zup"))
}

/// The frontend every fixture project declares.
///
/// The host's own backend decides: a GUI fixture on Linux would be refused
/// before any check the test cares about, so Linux fixtures declare the
/// console installer their backend ships.
fn selected_frontend() -> &'static str {
    if cfg!(windows) { "gui" } else { "console" }
}

/// The frontend fixture components are written for.
fn fixture_frontend() -> zup_core::Frontend {
    if cfg!(windows) {
        zup_core::Frontend::Gui
    } else {
        zup_core::Frontend::Console
    }
}

/// The application main a fixture declares, when the host's backend reads one.
///
/// Windows never validates it; Linux requires it to name an executable shipped
/// file, which these fixtures have no reason to do, so Linux fixtures declare
/// none.
fn fixture_main() -> &'static str {
    if cfg!(windows) {
        "main = \"app.exe\"\n"
    } else {
        ""
    }
}

/// A runtime template for `target`, written as a real zup component.
///
/// `doctor` reads a template's descriptor and its PE header and nothing else, so
/// a component whose header and descriptor agree is the whole of what these tests
/// need. No composition happens: `doctor` writes nothing.
fn setup_runtime(directory: &Path, target: &str) -> PathBuf {
    toolchain_fixture::runtime(target, fixture_frontend()).write(&directory.join("toolchain"))
}

fn manifest(target: &str) -> String {
    format!(
        r#"schema = 1
frontend = "{frontend}"

[app]
id = "com.example.doctor"
name = "Doctor App"
version = "1.0.0"
{main}
[build]

[build.targets.default]
target = "{target}"
source = {{ directory = "dist" }}

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/DoctorApp"

[[files]]
source = "**/*"
destination = "${{install}}"
"#,
        frontend = selected_frontend(),
        main = fixture_main(),
    )
}

fn write_project(root: &Path, manifest: &str) {
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::write(root.join("dist/app.bin"), b"payload").unwrap();
    fs::write(root.join("zup.toml"), manifest).unwrap();
}

fn single_target_project(target: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    write_project(project.path(), &manifest(target));
    project
}

/// Run `zup doctor` with the machine result on stdout, one flag per input.
fn run_doctor(manifest: &Path, inputs: &[(&str, &Path)]) -> Output {
    let mut command = zup();
    command
        .args(["doctor", "--manifest"])
        .arg(manifest)
        .args(["--format", "json"]);
    for (flag, path) in inputs {
        command.arg(flag).arg(path);
    }
    command.output().unwrap()
}

/// The whole document: the versioned envelope every operation command writes.
fn document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor stdout is not one JSON result: {error}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The doctor's own `details`, which is where the check table lives.
fn report(output: &Output) -> Value {
    let document = document(output);
    assert_eq!(document["operation"], "doctor", "{document}");
    assert_eq!(document["details"]["kind"], "doctor", "{document}");
    document["details"].clone()
}

/// The check rows for one profile, in report order.
fn checks(output: &Output, profile: &str) -> Vec<Value> {
    report(output)["targets"]
        .as_array()
        .expect("targets is an array")
        .iter()
        .find(|target| target["profile"] == profile)
        .unwrap_or_else(|| panic!("no report for profile {profile}"))["checks"]
        .as_array()
        .expect("checks is an array")
        .clone()
}

/// One profile's target row.
fn target_of(output: &Output, profile: &str) -> Value {
    report(output)["targets"]
        .as_array()
        .expect("targets is an array")
        .iter()
        .find(|target| target["profile"] == profile)
        .unwrap_or_else(|| panic!("no report for profile {profile}"))
        .clone()
}

fn find<'a>(rows: &'a [Value], kind: &str) -> &'a Value {
    rows.iter()
        .find(|check| check["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} check in {rows:?}"))
}

fn message(check: &Value) -> String {
    check["message"].as_str().unwrap_or_default().to_owned()
}

fn only(status: &str) -> BTreeSet<String> {
    BTreeSet::from([status.to_owned()])
}

fn statuses(rows: &[Value], kind: &str) -> BTreeSet<String> {
    rows.iter()
        .filter(|check| check["kind"] == kind)
        .map(|check| check["status"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn directory_entries(root: &Path) -> BTreeSet<String> {
    let mut entries = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            entries.insert(entry.path().display().to_string());
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    entries
}

fn plugin_component() -> Vec<u8> {
    let mut resolve = Resolve::default();
    let package = resolve.push_str("zup-plugin.wit", PLUGIN_WIT).unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    // `false` matches the encoder below, which leaves canonical names off.
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8, false).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

#[test]
fn healthy_single_target_passes_every_required_check() {
    let project = single_target_project(HOST_TARGET);
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let output = run_doctor(
        &project.path().join("zup.toml"),
        &[("--runtime", runtime.as_path())],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report(&output);
    assert_eq!(report["ready"], true);
    assert_eq!(report["host"], HOST_TARGET);
    assert_eq!(report["targets"].as_array().unwrap().len(), 1);

    let rows = checks(&output, "default");
    let kinds = rows
        .iter()
        .map(|check| check["kind"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(kinds, only_kinds());
    for check in &rows {
        assert!(
            check["message"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert!(check.get("path").is_some(), "{check}");
        assert_ne!(check["status"], "fail", "{check}");
    }
    assert_eq!(statuses(&rows, "plugin_engine"), only("skip"));
    assert_eq!(statuses(&rows, "update_root"), only("skip"));
    assert_eq!(statuses(&rows, "output_parent"), only("pass"));
    assert_eq!(
        find(&rows, "frontend")["message"],
        format!("resolved frontend is `{}`", selected_frontend())
    );
    assert!(
        message(find(&rows, "source_payload")).contains("payload digest"),
        "the payload summary carries a digest"
    );

    if ON_WINDOWS {
        assert_eq!(statuses(&rows, "runtime_template"), only("pass"));
        assert_eq!(statuses(&rows, "build_backend"), only("pass"));
        assert_eq!(statuses(&rows, "target_lowering"), only("pass"));
        assert!(
            message(find(&rows, "build_backend")).contains("Windows backend is ready"),
            "{}",
            message(find(&rows, "build_backend"))
        );
        assert_eq!(report["ready"], true);
    } else {
        assert_eq!(statuses(&rows, "runtime_template"), only("pass"));
        assert_eq!(statuses(&rows, "build_backend"), only("pass"));
        assert_eq!(statuses(&rows, "target_lowering"), only("pass"));
        assert!(
            message(find(&rows, "build_backend")).contains("Linux backend is ready"),
            "{}",
            message(find(&rows, "build_backend"))
        );
        assert_eq!(report["ready"], true);
    }

    // The ordinary case: nobody passed `--runtime` at all, and the report still
    // says where the template came from. A readiness report that needed a path to
    // say anything about the runtime would be a report about the developer's
    // typing, and the resolver is what a person who installed `zup` depends on.
    let resolved = single_target_project(HOST_TARGET);
    let output = run_doctor(&resolved.path().join("zup.toml"), &[]);
    let rows = checks(&output, "default");
    let check = find(&rows, "runtime_template");
    assert_eq!(check["status"], "pass", "{}", message(check));
    assert!(message(check).contains("is a zup "), "{}", message(check));
    assert!(
        ["staged", "cache", "toolchain root"]
            .iter()
            .any(|source| message(check).contains(source)),
        "the report says where the component came from: {}",
        message(check)
    );
}

fn only_kinds() -> BTreeSet<String> {
    CHECK_KINDS.iter().map(|kind| (*kind).to_owned()).collect()
}

/// A `--runtime` a person typed is checked exactly like one the resolver found, so
/// a wrong path and a stale toolchain produce the same report rather than two
/// different shapes of failure. And a cardinality mismatch is a diagnostic rather
/// than an argument error: every other check still runs.
#[test]
fn a_bad_runtime_is_reported_without_stopping_other_checks() {
    let project = single_target_project(HOST_TARGET);
    let missing = project.path().join("absent-runtime.exe");
    let output = run_doctor(
        &project.path().join("zup.toml"),
        &[("--runtime", missing.as_path())],
    );
    assert!(!output.status.success());
    let rows = checks(&output, "default");
    let template = find(&rows, "runtime_template");
    assert_eq!(template["status"], "fail", "{template}");
    assert_eq!(template["path"], missing.display().to_string());
    let message = message(template);
    assert!(message.contains("cargo xtask toolchain build"), "{message}");
    for kind in [
        "output_parent",
        "source_payload",
        "canonical_target",
        "frontend",
    ] {
        assert_eq!(statuses(&rows, kind), only("pass"), "{kind}");
    }
}

#[test]
fn one_runtime_per_target_is_required_and_reported_for_each_profile() {
    let project = single_target_project(HOST_TARGET);
    let source = fs::read_to_string(project.path().join("zup.toml")).unwrap();
    let default_table = format!(
        "[build.targets.default]\ntarget = \"{HOST_TARGET}\"\nsource = {{ directory = \"dist\" }}"
    );
    let matrix_table = format!(
        "[build.targets.alpha]\ntarget = \"{HOST_TARGET}\"\nsource = {{ directory = \"dist\" }}\n\n[build.targets.beta]\ntarget = \"{OTHER_TARGET}\"\nsource = {{ directory = \"dist\" }}"
    );
    assert!(source.contains(&default_table), "the fixture changed shape");
    fs::write(
        project.path().join("zup.toml"),
        source.replace(&default_table, &matrix_table),
    )
    .unwrap();
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let output = run_doctor(
        &project.path().join("zup.toml"),
        &[("--runtime", runtime.as_path())],
    );
    assert!(!output.status.success());
    let report = report(&output);
    let profiles = report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| target["profile"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(profiles, ["alpha", "beta"], "selection order is stable");
    for profile in profiles {
        let rows = checks(&output, &profile);
        let cardinality = rows
            .iter()
            .find(|check| {
                check["kind"] == "runtime_template"
                    && message(check).contains("provide one --runtime per target")
            })
            .unwrap_or_else(|| panic!("{profile} has no cardinality diagnostic: {rows:?}"));
        assert_eq!(cardinality["status"], "fail");
        // Cardinality is a diagnostic, not an argument error: the rest still ran.
        assert_eq!(statuses(&rows, "output_parent"), only("pass"), "{profile}");
        assert_eq!(
            statuses(&rows, "canonical_target"),
            only("pass"),
            "{profile}"
        );
    }
}

#[test]
fn all_target_matrix_checks_every_selected_profile() {
    let project = TempDir::new().unwrap();
    for name in ["alpha", "beta"] {
        fs::create_dir_all(project.path().join(format!("dist/{name}"))).unwrap();
        fs::write(
            project.path().join(format!("dist/{name}/app.bin")),
            name.as_bytes(),
        )
        .unwrap();
    }
    fs::write(
        project.path().join("zup.toml"),
        format!(
            r#"schema = 1
frontend = "{frontend}"

[app]
id = "com.example.doctor-matrix"
name = "Doctor Matrix"
version = "2.1.0"

[build]

[build.targets.alpha]
target = "{HOST_TARGET}"
source = {{ directory = "dist/alpha" }}

[build.targets.beta]
target = "{OTHER_TARGET}"
source = {{ directory = "dist/beta" }}

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/DoctorMatrix"

[[files]]
source = "**/*"
destination = "${{install}}"
"#,
            frontend = selected_frontend()
        ),
    )
    .unwrap();

    // The second target is handed a template built for the first one, so its
    // runtime check has something real to fail on. The copy carries its
    // descriptor, so the failure is about the machine and not about a missing file.
    let alpha_runtime = setup_runtime(project.path(), HOST_TARGET);
    let second = toolchain_fixture::runtime(HOST_TARGET, fixture_frontend())
        .write(&project.path().join("other-template"));
    let alpha_output = project.path().join("alpha.exe");
    let beta_output = project.path().join("beta.exe");
    let output = run_doctor(
        &project.path().join("zup.toml"),
        &[
            ("--runtime", alpha_runtime.as_path()),
            ("--runtime", second.as_path()),
            ("--output", alpha_output.as_path()),
            ("--output", beta_output.as_path()),
        ],
    );
    let report = report(&output);
    assert_eq!(report["targets"].as_array().unwrap().len(), 2);

    let alpha = checks(&output, "alpha");
    let beta = checks(&output, "beta");
    assert_eq!(target_of(&output, "alpha")["target"], HOST_TARGET);
    assert_eq!(target_of(&output, "beta")["target"], OTHER_TARGET);
    for rows in [&alpha, &beta] {
        assert_eq!(
            rows.iter()
                .map(|check| check["kind"].as_str().unwrap().to_owned())
                .collect::<BTreeSet<_>>(),
            only_kinds()
        );
        // Both targets report a payload and an output even when a runtime is wrong.
        assert_eq!(statuses(rows, "source_payload"), only("pass"));
        assert_eq!(statuses(rows, "output_parent"), only("pass"));
    }
    assert_eq!(
        find(&alpha, "output_parent")["path"],
        alpha_output.display().to_string()
    );
    assert_eq!(
        find(&beta, "output_parent")["path"],
        beta_output.display().to_string()
    );

    if ON_WINDOWS {
        // The second target was handed the first one's template, so the check that
        // owns the disagreement fails and the other rows still report.
        assert_eq!(statuses(&alpha, "runtime_template"), only("pass"));
        assert_eq!(statuses(&beta, "runtime_template"), only("fail"));
        let message = message(find(&beta, "runtime_template"));
        assert!(message.contains(OTHER_TARGET), "{message}");
        assert!(message.contains("was wanted"), "{message}");
        assert!(!output.status.success());
    }
}

/// One check failing must not hide the others. `doctor`'s value is that a person
/// sees everything wrong with a project in one report, so each of these breaks a
/// different single thing and asserts that the checks which do not depend on it
/// still report - passing where they can, skipping where the break made the
/// question unanswerable.
#[test]
fn one_broken_thing_leaves_every_independent_check_reporting() {
    // A missing source root. Nothing can be lowered or composed without a payload,
    // so those skip; nothing about the target, the frontend or the output is
    // affected, so those pass.
    let project = single_target_project(HOST_TARGET);
    fs::remove_dir_all(project.path().join("dist")).unwrap();
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let output = run_doctor(
        &project.path().join("zup.toml"),
        &[("--runtime", runtime.as_path())],
    );
    assert!(!output.status.success());
    let rows = checks(&output, "default");
    assert_eq!(statuses(&rows, "source_payload"), only("fail"));
    assert!(
        message(find(&rows, "source_payload")).contains("does not exist"),
        "{}",
        message(find(&rows, "source_payload"))
    );
    for kind in ["manifest_compile", "plugin_engine", "target_lowering"] {
        assert_eq!(statuses(&rows, kind), only("skip"), "{kind}");
    }
    for kind in ["output_parent", "frontend", "canonical_target"] {
        assert_eq!(statuses(&rows, kind), only("pass"), "{kind}");
    }

    // An unsupported target. The backend is a finding, and no artifact is produced.
    let project = single_target_project(UNSUPPORTED_TARGET);
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let result = run_doctor(
        &project.path().join("zup.toml"),
        &[("--runtime", runtime.as_path())],
    );
    assert!(!result.status.success());
    let report = report(&result);
    assert_eq!(report["ready"], false);
    let rows = checks(&result, "default");
    assert_eq!(report["targets"][0]["target"], UNSUPPORTED_TARGET);
    if ON_WINDOWS {
        // An unsupported Linux triple on a Windows host: the backend exists,
        // so the finding names the supported target rather than the host.
        assert_eq!(statuses(&rows, "build_backend"), only("pass"));
        assert_eq!(statuses(&rows, "target_lowering"), only("fail"));
        assert!(
            message(find(&rows, "target_lowering")).contains("x86_64-unknown-linux-gnu"),
            "{}",
            message(find(&rows, "target_lowering"))
        );
    } else {
        assert_eq!(statuses(&rows, "build_backend"), only("fail"));
        let backend = find(&rows, "build_backend");
        // A Windows target has an implemented backend, so the finding is about
        // *this host* not being able to run it. "not implemented" would be a
        // different claim: that no backend answers for the platform anywhere.
        assert!(
            message(backend).contains("backend unavailable"),
            "{}",
            message(backend)
        );
        assert_eq!(statuses(&rows, "target_lowering"), only("skip"));
    }
    for kind in [
        "manifest_compile",
        "source_payload",
        "frontend",
        "canonical_target",
        "output_parent",
    ] {
        assert_eq!(statuses(&rows, kind), only("pass"), "{kind}");
    }
    // The derived output carries the target's own suffix, so the assertion
    // names the file the target would have produced.
    let suffix = zup_core::TargetTriple::parse(UNSUPPORTED_TARGET)
        .expect("a valid target")
        .executable_suffix();
    assert!(
        !project
            .path()
            .join(format!("Doctor App-Setup{suffix}"))
            .exists(),
        "no artifact is produced for an unsupported target"
    );
}

#[test]
fn plugin_and_update_root_checks_are_typed() {
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::create_dir_all(project.path().join("keys")).unwrap();
    fs::create_dir_all(project.path().join("plugins")).unwrap();
    fs::write(project.path().join("dist/app.bin"), b"payload").unwrap();
    fs::write(
        project.path().join("plugins/helper.wasm"),
        plugin_component(),
    )
    .unwrap();
    fs::write(project.path().join("keys/root.json"), b"{\"signed\":{}}").unwrap();
    fs::write(
        project.path().join("zup.toml"),
        format!(
            r#"schema = 1
frontend = "{frontend}"

[app]
id = "com.example.doctor-plugins"
name = "Doctor Plugins"
version = "1.0.0"

[build]

[build.targets.default]
target = "{HOST_TARGET}"
source = {{ directory = "dist" }}

[updates]
repository = "https://updates.example.com/acme"
channel = "stable"
root = "keys/root.json"

[install]
scope = "user"

[install.directory]
user = "${{location.user_data}}/DoctorPlugins"

[[files]]
source = "**/*"
destination = "${{install}}"

[[plugins]]
id = "helper"
source = "plugins/helper.wasm"
"#,
            frontend = selected_frontend()
        ),
    )
    .unwrap();

    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let manifest = project.path().join("zup.toml");
    let output = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = checks(&output, "default");
    let plugins = find(&rows, "plugin_engine");
    assert_eq!(plugins["status"], "pass", "{}", message(plugins));
    assert!(
        message(plugins).contains("compiled and verified with Wasmtime"),
        "{}",
        message(plugins)
    );
    let updates = find(&rows, "update_root");
    assert_eq!(updates["status"], "pass", "{}", message(updates));
    assert!(
        message(updates).contains("channel stable"),
        "{}",
        message(updates)
    );
    assert!(
        updates["path"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("keys/root.json"),
        "{}",
        updates["path"]
    );

    // A source that is not a component fails only the plugin check.
    fs::write(
        project.path().join("plugins/helper.wasm"),
        b"not a component",
    )
    .unwrap();
    let broken = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    assert!(!broken.status.success());
    let rows = checks(&broken, "default");
    let plugins = find(&rows, "plugin_engine");
    assert_eq!(plugins["status"], "fail", "{}", message(plugins));
    assert!(
        message(plugins).contains("plugin AOT compilation is not ready"),
        "the plugin diagnostic explains the failure: {}",
        message(plugins)
    );
    assert_eq!(statuses(&rows, "update_root"), only("pass"));
    assert_eq!(statuses(&rows, "source_payload"), only("pass"));

    // A missing trusted root is owned by the update check; the plan cannot exist
    // without it, so the plan-dependent checks are skipped, not failed.
    fs::write(
        project.path().join("plugins/helper.wasm"),
        plugin_component(),
    )
    .unwrap();
    fs::remove_file(project.path().join("keys/root.json")).unwrap();
    let untrusted = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    assert!(!untrusted.status.success());
    let rows = checks(&untrusted, "default");
    let updates = find(&rows, "update_root");
    assert_eq!(updates["status"], "fail", "{}", message(updates));
    assert!(
        message(updates).contains("root.json"),
        "{}",
        message(updates)
    );
    for kind in ["plugin_engine", "source_payload", "target_lowering"] {
        assert_eq!(statuses(&rows, kind), only("skip"), "{kind}");
    }
    for kind in ["canonical_target", "output_parent", "runtime_template"] {
        assert_eq!(statuses(&rows, kind), only("pass"), "{kind}");
    }
}

/// The document names the contract it belongs to, so a consumer gates on a version
/// rather than on a shape it has to recognise - and the same report twice is
/// byte-identical, so a CI diff means something changed.
#[test]
fn the_result_is_versioned_and_byte_stable() {
    let project = single_target_project(HOST_TARGET);
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let manifest = project.path().join("zup.toml");
    let first = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    let second = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    assert_eq!(first.stdout, second.stdout, "the report is deterministic");

    // The human report moves to stderr rather than disappearing: stdout is the
    // protocol, and a CI log that swallowed the table would be useless to the person
    // reading it.
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        stderr.contains("ready: all 1 target(s) can be built"),
        "{stderr}"
    );
    let document = document(&first);
    assert_eq!(document["protocol"], zup_automation::PROTOCOL.to_string());
    assert_eq!(document["operation"], "doctor");
    assert_eq!(document["status"], "success");
    for target in document["details"]["targets"].as_array().unwrap() {
        for check in target["checks"].as_array().unwrap() {
            assert!(["pass", "fail", "skip"].contains(&check["status"].as_str().unwrap()));
        }
    }
}

/// The report describes the source it would use, including a CLI override.
#[test]
fn the_report_honors_a_cli_source_override() {
    let project = single_target_project(HOST_TARGET);
    fs::create_dir_all(project.path().join("out/cli")).unwrap();
    fs::write(project.path().join("out/cli/app.bin"), b"cli payload").unwrap();
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let manifest = project.path().join("zup.toml");
    // A source is resolved against the project, not the working directory.
    let result = run_doctor(
        &manifest,
        &[
            ("--runtime", runtime.as_path()),
            ("--source", Path::new("out/cli")),
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = checks(&result, "default");
    let payload = find(&rows, "source_payload");
    assert_eq!(payload["status"], "pass", "{}", message(payload));
    assert!(
        payload["path"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("out/cli"),
        "the payload was read from the overridden source: {}",
        payload["path"]
    );

    // A source that does not exist is reported against the override.
    let missing = run_doctor(
        &manifest,
        &[
            ("--runtime", runtime.as_path()),
            ("--source", Path::new("out/absent")),
        ],
    );
    assert!(!missing.status.success());
    let rows = checks(&missing, "default");
    assert_eq!(statuses(&rows, "source_payload"), only("fail"));
    assert!(
        message(find(&rows, "source_payload")).contains("does not exist"),
        "{}",
        message(find(&rows, "source_payload"))
    );
    assert_eq!(statuses(&rows, "output_parent"), only("pass"));
}

/// `doctor` reports, it never writes. Every `--output` state a build can be in -
/// derivable, parent missing, already present - is a diagnostic here, and none of
/// them touches the filesystem.
#[test]
fn doctor_reports_every_output_state_and_never_writes() {
    let project = single_target_project(HOST_TARGET);
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let manifest = project.path().join("zup.toml");
    // The derived output carries the target's own suffix: extensionless on
    // Linux, `.exe` on Windows.
    let suffix = zup_core::TargetTriple::parse(HOST_TARGET)
        .expect("a valid target")
        .executable_suffix();
    let derived = project.path().join(format!("Doctor App-Setup{suffix}"));
    let before = directory_entries(project.path());
    let result = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!derived.exists(), "doctor must not create the installer");
    assert_eq!(before, directory_entries(project.path()));
    assert_eq!(
        fs::read(project.path().join("dist/app.bin")).unwrap(),
        b"payload"
    );

    // An output path whose parent is missing is reported, not created.
    let chosen = project.path().join("nested/Setup.exe");
    let result = run_doctor(
        &manifest,
        &[
            ("--runtime", runtime.as_path()),
            ("--output", chosen.as_path()),
        ],
    );
    assert!(!result.status.success(), "a missing parent is not ready");
    let rows = checks(&result, "default");
    let output_check = find(&rows, "output_parent");
    assert_eq!(output_check["status"], "fail", "{}", message(output_check));
    assert!(
        message(output_check).contains("does not exist"),
        "{}",
        message(output_check)
    );
    assert!(!chosen.exists());
    assert!(!chosen.parent().unwrap().exists());

    // An existing output is reported, and left byte-for-byte alone.
    let output = project.path().join("Setup.exe");
    fs::write(&output, b"previous build").unwrap();
    let result = run_doctor(
        &manifest,
        &[
            ("--runtime", runtime.as_path()),
            ("--output", output.as_path()),
        ],
    );
    assert!(!result.status.success());
    let rows = checks(&result, "default");
    let check = rows
        .iter()
        .find(|check| check["kind"] == "output_parent" && message(check).contains("already exists"))
        .unwrap_or_else(|| panic!("no existing-output diagnostic: {rows:?}"));
    assert_eq!(check["status"], "fail");
    assert!(
        message(check).contains("--force"),
        "the finding names the escape hatch: {}",
        message(check)
    );
    assert_eq!(
        fs::read(&output).unwrap(),
        b"previous build",
        "doctor overwrites nothing"
    );
}

#[test]
fn a_manifest_that_cannot_be_read_is_a_hard_error() {
    let project = TempDir::new().unwrap();
    let manifest = project.path().join("zup.toml");
    fs::write(&manifest, "schema = 1\n").unwrap();
    let runtime = setup_runtime(project.path(), HOST_TARGET);
    let output = run_doctor(&manifest, &[("--runtime", runtime.as_path())]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("zup.toml"), "{stderr}");
    let document = document(&output);
    assert_eq!(document["status"], "failure");
    assert!(
        document["details"].is_null(),
        "no report is invented for a manifest that cannot be parsed: {document}"
    );
    let diagnostic = &document["diagnostics"][0];
    assert_eq!(diagnostic["severity"], "error");
    assert!(
        diagnostic["source"]["file"]
            .as_str()
            .is_some_and(|file| file.ends_with("zup.toml")),
        "the diagnostic points at the file it could not read: {diagnostic}"
    );
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|text| text.contains("missing field `app`")),
        "{diagnostic}"
    );
}
