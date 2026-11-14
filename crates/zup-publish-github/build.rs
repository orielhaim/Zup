use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn valid(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn main() {
    println!("cargo:rerun-if-env-changed=ZUP_ACTION_REVISION");
    let crate_root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = crate_root.join("../..").canonicalize().unwrap();
    let vcs = crate_root.join(".cargo_vcs_info.json");
    if vcs.is_file() {
        println!("cargo:rerun-if-changed={}", vcs.display());
    }
    let source_root = git(&root, &["rev-parse", "--show-toplevel"])
        .and_then(|p| PathBuf::from(p).canonicalize().ok());
    let revision = (|| {
        if source_root.as_ref() != Some(&root) {
            return None;
        }
        for name in [
            Some("HEAD".to_owned()),
            Some("packed-refs".to_owned()),
            git(&root, &["symbolic-ref", "-q", "HEAD"]),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(path) = git(&root, &["rev-parse", "--git-path", &name]) {
                let path = root.join(path);
                if path.is_file() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])
    })()
    .or_else(|| {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(vcs).ok()?).ok()?;
        value["git"]["sha1"].as_str().map(str::to_owned)
    });
    let configured = std::env::var("ZUP_ACTION_REVISION").ok();
    if let (Some(actual), Some(configured)) = (&revision, &configured) {
        assert_eq!(
            actual, configured,
            "ZUP_ACTION_REVISION differs from source provenance"
        );
    }
    if let Some(revision) = revision.or(configured) {
        assert!(
            valid(&revision),
            "Zup action provenance must be a full lowercase commit SHA"
        );
        println!("cargo:rustc-env=ZUP_PUBLISH_SOURCE_REVISION={revision}");
    } else {
        println!("cargo:rustc-env=ZUP_PUBLISH_SOURCE_REVISION=");
    }
}
