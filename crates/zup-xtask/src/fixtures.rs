use std::path::Path;
use std::process::Command;

pub fn build(root: &Path) -> Result<(), String> {
    use zup_plugin_abi::test_fixture::{digest, input_digest};
    let before = input_digest(root)?;
    let output = root.join("target/test-fixtures");
    let cargo_target = output.join("cargo");
    let build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--message-format=json",
            "--release",
            "--features",
            "sandbox-probes",
            "--target",
            "wasm32-wasip3",
            "--manifest-path",
        ])
        .arg(root.join("examples/plugins/configure/Cargo.toml"))
        .arg("--target-dir")
        .arg(&cargo_target)
        .output()
        .map_err(|e| format!("building the SDK plugin fixture: {e}"))?;
    let mut artifact = None;
    for message in cargo_metadata::Message::parse_stream(build.stdout.as_slice()) {
        match message.map_err(|e| e.to_string())? {
            cargo_metadata::Message::CompilerArtifact(compiled)
                if compiled.target.is_cdylib() && compiled.target.name == "configure" =>
            {
                artifact = compiled
                    .filenames
                    .into_iter()
                    .find(|path| path.extension() == Some("wasm"));
            }
            cargo_metadata::Message::CompilerMessage(message) => {
                if let Some(rendered) = message.message.rendered {
                    eprint!("{rendered}");
                }
            }
            _ => {}
        }
    }
    if !build.status.success() {
        return Err(format!(
            "plugin fixture build failed: {}",
            String::from_utf8_lossy(&build.stderr)
        ));
    }
    let artifact = artifact.ok_or("Cargo did not emit the configure component")?;
    let component = std::fs::read(artifact).map_err(|e| e.to_string())?;
    zup_plugin_contract::PluginEngine::host()
        .map_err(|e| e.to_string())?
        .compile_component(&component)
        .map_err(|e| e.to_string())?;
    if input_digest(root)? != before {
        return Err("plugin fixture inputs changed during compilation".into());
    }
    std::fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    publish(&output.join("configure.wasm"), &component)?;
    publish(
        &output.join("configure.sha256"),
        format!("{before}\n{}\n", digest(&component)).as_bytes(),
    )?;
    Ok(())
}

fn publish(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(|e| e.to_string())?;
    temporary.write_all(bytes).map_err(|e| e.to_string())?;
    temporary.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}
