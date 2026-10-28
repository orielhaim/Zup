use std::path::{Path, PathBuf};

use zup_bundle::Package;
use zup_compose::ComposeError;
use zup_core::Target;

#[derive(Debug)]
pub struct Carrier {
    executable: PathBuf,
    package: Package,
    preset: Option<Vec<u8>>,
}

impl Carrier {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ComposeError> {
        let executable = path.as_ref().to_path_buf();
        let opened = zup_compose::open_linux(&executable)?;
        Ok(Self {
            executable,
            package: opened.package,
            preset: opened.preset,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn package(&self) -> &Package {
        &self.package
    }

    pub fn preset(&self) -> Option<&[u8]> {
        self.preset.as_deref()
    }

    pub fn into_package(self) -> Package {
        self.package
    }

    pub fn target(&self) -> &Target {
        &self.package.plan().installer.target
    }
}
