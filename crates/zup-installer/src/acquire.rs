pub use zup_update::content::*;

use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub action: Option<zup_exec::LifecycleAction>,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub install_directory: Option<PathBuf>,
    pub scope: Option<zup_core::SelectedScope>,
}

impl Request {
    pub fn new(action: zup_exec::LifecycleAction) -> Self {
        Self {
            action: Some(action),
            ..Self::default()
        }
    }

    pub fn components(mut self, enable: &[String], disable: &[String]) -> Self {
        self.enable = enable.to_vec();
        self.disable = disable.to_vec();
        self
    }
}
