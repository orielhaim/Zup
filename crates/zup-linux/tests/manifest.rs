#![cfg(target_os = "linux")]
#![cfg(feature = "test-support")]

use zup_core::{SelectedScope, TargetTriple};

use zup_linux::test_support::{
    IsolatedUser, compose_installer, genuine_template, run_installer_process, run_tool,
};

const MANIFEST: &str = r#"
schema = 1

[app]
id = "com.example.tool"
name = "Tool"
version = "1.0.0"

[build]

[build.targets.linux]
target = "x86_64-unknown-linux-gnu"
frontend = "console"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.programs}/tool"

[[components]]
id = "core"
name = "Core"
required = true

[[files]]
source = "tool"
destination = "${install}"
component = "core"
executable = true

[[files]]
source = "keep.dat"
destination = "${install}"
component = "core"
"#;

const TOOL: &str = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"tool 1.0.0\"; exit 0; fi\necho \"tool: unknown command $1\" >&2\nexit 1\n";

#[test]
fn manifest_to_running_application() {
    let user = IsolatedUser::isolate();
    let project = tempfile::tempdir().expect("a project directory");
    std::fs::write(project.path().join("zup.toml"), MANIFEST).expect("a manifest");
    let dist = project.path().join("dist");
    std::fs::create_dir_all(&dist).expect("a source directory");
    std::fs::write(dist.join("tool"), TOOL).expect("the tool source");
    std::fs::write(dist.join("keep.dat"), b"keep-v1").expect("the data source");

    let manifest_path = project.path().join("zup.toml");
    let manifest =
        zup_manifest::parse(&std::fs::read_to_string(&manifest_path).expect("the manifest reads"))
            .expect("the manifest parses");

    let target = TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a Linux target");
    let config = zup_core::ResolvedTargetConfig {
        profile: zup_core::TargetProfileId::new("linux").expect("a profile id"),
        target: target.clone(),
        source: zup_core::Source {
            directory: "dist".into(),
        },
        frontend: zup_core::Frontend::Console,
        install: manifest.install.clone(),
    };
    let installer =
        zup_manifest::compile(&manifest, &config, &zup_core::TargetOverrides::default())
            .expect("the manifest compiles");
    assert_eq!(installer.target, target);

    let build = zup_build::materialize_with_policy(
        &manifest_path,
        &manifest,
        vec![(config, installer)],
        &zup_linux::LinuxSourceFilePolicy,
        zup_build::Writes::None,
    )
    .expect("the source materializes");
    assert_eq!(build.targets.len(), 1);
    assert_eq!(
        build.targets[0].files.len(),
        2,
        "both payload files materialize"
    );

    let package =
        zup_bundle::BundleWriter::encode(&build.targets[0], &[]).expect("the package encodes");

    {
        let decoded = zup_bundle::Package::from_bytes(package.clone()).expect("a package decodes");
        let plan = decoded.build_plan().expect("a plan decodes");
        let tool = plan.targets[0]
            .files
            .iter()
            .find(|file| file.source_relative.as_str() == "tool")
            .expect("the tool round-trips");
        assert!(tool.executable, "executable intent survives the package");
    }
    let output = project.path().join("Acme-Setup");
    compose_installer(&genuine_template("console"), &output, &package);

    let result = run_installer_process(&output, &user, &[]);
    assert!(
        result.status.success(),
        "install exit {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );

    let install = user.programs().join("tool");
    assert_eq!(
        run_tool(&install.join("tool"), &["--version"]).trim(),
        "tool 1.0.0",
        "the manifest-built application runs"
    );
    assert_eq!(
        std::fs::read(install.join("keep.dat")).expect("keep.dat"),
        b"keep-v1"
    );
    let _ = SelectedScope::User;
}
