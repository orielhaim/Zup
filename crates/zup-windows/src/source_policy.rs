use std::io;
use std::path::Path;

use zup_platform::SourceFilePolicy;

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsSourceFilePolicy;

impl SourceFilePolicy for WindowsSourceFilePolicy {
    fn is_link(&self, path: &Path) -> Result<bool, io::Error> {
        crate::bindings::is_link_or_reparse(path)
    }
}
