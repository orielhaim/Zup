#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Junction {
    path: PathBuf,
}

impl Junction {
    pub fn new(link: &Path, target: &Path) -> Self {
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
