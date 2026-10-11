use std::path::Path;

pub fn ensure_ancestor_chain<E>(
    path: &Path,
    mut ensure_one: impl FnMut(&Path) -> Result<(), E>,
) -> Result<(), E> {
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        ensure_one(ancestor)?;
    }
    Ok(())
}

pub fn verify_within_base<E>(
    base: &Path,
    path: &Path,
    escape: impl FnOnce() -> E,
    mut verify_one: impl FnMut(&Path) -> Result<(), E>,
) -> Result<(), E> {
    let relative = path.strip_prefix(base).map_err(|_| escape())?;
    let mut current = base.to_path_buf();
    verify_one(&current)?;
    for component in relative.components() {
        current.push(component);
        verify_one(&current)?;
    }
    Ok(())
}

pub fn is_real_dir(path: &Path, metadata: &std::fs::Metadata) -> bool {
    metadata.is_dir() && !crate::bindings::is_link_metadata(metadata, path)
}

pub fn is_real_file(path: &Path, metadata: &std::fs::Metadata) -> bool {
    metadata.is_file() && !crate::bindings::is_link_metadata(metadata, path)
}
