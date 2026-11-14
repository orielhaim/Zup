use zup_core::Template;

use crate::{DesiredFile, DesiredState, TargetPath};

pub fn resolve_files<E>(
    desired: &DesiredState,
    mut resolve: impl FnMut(&Template, &'static str) -> Result<TargetPath, E>,
) -> Result<Vec<DesiredFile<TargetPath>>, E> {
    desired
        .files
        .iter()
        .map(|file| {
            Ok(DesiredFile {
                key: file.key.clone(),
                placement: file
                    .placement
                    .resolve(|path| resolve(path, "file destination"))?,
                source_relative: file.source_relative.clone(),
                size: file.size,
                sha256: file.sha256,
                executable: file.executable,
                privilege: file.privilege,
            })
        })
        .collect()
}
