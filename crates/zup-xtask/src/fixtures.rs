use std::path::Path;
use std::process::Command;

pub fn build(root: &Path) -> Result<(), String> {
    use zup_plugin_abi::test_fixture::{digest, input_digest};
    let before = input_digest(root)?;
    let output = root.join("target/test-fixtures");
    let cargo_target = output.join("cargo");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--manifest-path",
        ])
        .arg(root.join("examples/plugins/configure/Cargo.toml"))
        .arg("--target-dir")
        .arg(&cargo_target)
        .status()
        .map_err(|e| format!("building the SDK plugin fixture: {e}"))?;
    if !status.success() {
        return Err(
            "plugin fixture build failed; install the wasm32-unknown-unknown Rust target".into(),
        );
    }
    let module = std::fs::read(cargo_target.join("wasm32-unknown-unknown/release/configure.wasm"))
        .map_err(|e| e.to_string())?;
    let component = zup_plugin_build::componentize(&module).map_err(|e| e.to_string())?;
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
