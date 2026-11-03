use zup_core::{ResourceKey, Template};

use crate::{DesiredFile, DesiredState, TargetPath};

pub fn resolve_files<E>(
    desired: &DesiredState,
    mut resolve: impl FnMut(&Template, &'static str) -> Result<TargetPath, E>,
) -> Result<Vec<DesiredFile<TargetPath>>, E> {
    desired
        .files
        .iter()
        .map(|file| {
            let destination = resolve(&file.destination, "file destination")?;
            Ok(DesiredFile {
                key: ResourceKey::File {
                    destination: destination.to_string(),
                },
                destination,
                source_relative: file.source_relative.clone(),
                size: file.size,
                sha256: file.sha256,
                executable: file.executable,
                privilege: file.privilege,
            })
        })
        .collect()
}
