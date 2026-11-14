use std::os::unix::fs::MetadataExt;

pub(crate) fn validate_init_view() -> Result<(), String> {
    for (local, init) in [
        ("/", "/proc/1/root"),
        ("/proc/self/ns/mnt", "/proc/1/ns/mnt"),
        ("/proc/self/ns/pid", "/proc/1/ns/pid"),
        ("/proc/self/ns/user", "/proc/1/ns/user"),
    ] {
        same_object(local, init)?;
    }
    Ok(())
}

pub(crate) fn same_object(local: &str, init: &str) -> Result<(), String> {
    let local = std::fs::metadata(local).map_err(|error| error.to_string())?;
    let init = std::fs::metadata(init).map_err(|error| error.to_string())?;
    if (local.dev(), local.ino()) != (init.dev(), init.ino()) {
        return Err("native service control requires init's filesystem and namespaces".into());
    }
    Ok(())
}
