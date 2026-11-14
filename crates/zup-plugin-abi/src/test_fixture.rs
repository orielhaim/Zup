use std::path::{Path, PathBuf};

pub fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}

pub fn input_digest(root: &Path) -> Result<String, String> {
    let mut inputs = Vec::new();
    inputs.push(root.join("Cargo.toml"));
    inputs.push(root.join("rust-toolchain.toml"));
    inputs.push(root.join("crates/zup-xtask/src/fixtures.rs"));
    for name in [
        "examples/plugins/configure",
        "crates/zup-sdk",
        "crates/zup-plugin-abi",
    ] {
        let directory = root.join(name);
        inputs.push(directory.join("Cargo.toml"));
        if directory.join("build.rs").exists() {
            inputs.push(directory.join("build.rs"));
        }
        collect(&directory.join("src"), &mut inputs)?;
    }
    inputs.push(root.join("examples/plugins/configure/Cargo.lock"));
    inputs.push(root.join("crates/zup-plugin-abi/wit/zup-plugin.wit"));
    inputs.sort();
    let mut bytes = Vec::new();
    for path in inputs {
        let name = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        bytes.extend_from_slice(text.replace("\r\n", "\n").as_bytes());
        bytes.push(0);
    }
    Ok(digest(&bytes))
}

fn collect(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("fixture inputs cannot be symlinks".into());
        }
        if kind.is_dir() {
            collect(&entry.path(), paths)?;
        } else if kind.is_file() {
            paths.push(entry.path());
        }
    }
    Ok(())
}

pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn component() -> Vec<u8> {
    let root = repository();
    let directory = root.join("target/test-fixtures");
    let load = || -> Result<Vec<u8>, String> {
        let component =
            std::fs::read(directory.join("configure.wasm")).map_err(|e| e.to_string())?;
        let proof = std::fs::read_to_string(directory.join("configure.sha256"))
            .map_err(|e| e.to_string())?;
        if proof != format!("{}\n{}\n", input_digest(&root)?, digest(&component)) {
            return Err("fixture inputs or component changed".into());
        }
        Ok(component)
    };
    load().unwrap_or_else(|error| {
        panic!("plugin fixture unavailable or stale: {error}; run `cargo xtask fixtures build`")
    })
}
