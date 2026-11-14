use std::{
    collections::BTreeMap,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const DIRECTORIES: &[&str] = &[
    "/etc/systemd/system/service.d",
    "/run/systemd/system/service.d",
    "/usr/local/lib/systemd/system/service.d",
    "/usr/lib/systemd/system/service.d",
    "/lib/systemd/system/service.d",
];

fn roots() -> Result<Vec<PathBuf>, String> {
    DIRECTORIES
        .iter()
        .filter_map(|path| match std::fs::canonicalize(path) {
            Ok(path) => Some(Ok(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(Err(error.to_string())),
        })
        .collect()
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    let parent = path
        .parent()
        .ok_or("global service policy lacks a directory")?;
    for ancestor in parent.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor).map_err(|error| error.to_string())?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "untrusted global service policy directory {}",
                ancestor.display()
            ));
        }
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid global policy filename")?;
    let directory = crate::fs::OwnedDirectory::open(parent).map_err(|error| error.to_string())?;
    let file = directory
        .open_regular_read(name)
        .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.len() > 65536 {
        return Err("untrusted or oversized global service policy".into());
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 65536 {
        return Err("global service policy grew beyond its bound".into());
    }
    Ok(bytes)
}

pub fn generation() -> Result<String, String> {
    let mut files = BTreeMap::new();
    for directory in roots()? {
        for entry in std::fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "conf")
            {
                files.insert(path.clone(), read(&path)?);
                if files.len() > 256 {
                    return Err("too many global service policies".into());
                }
            }
        }
    }
    let bytes = serde_json::to_vec(&files).map_err(|error| error.to_string())?;
    Ok(zup_core::hash_bytes(&bytes).to_hex())
}

pub fn validate(paths: &[String]) -> Result<(), String> {
    stop_failure_mode(paths).map(|_| ())
}

pub fn stop_failure_mode(paths: &[String]) -> Result<&'static str, String> {
    let roots = roots()?;
    let mut paths = paths.iter().map(Path::new).collect::<Vec<_>>();
    paths.sort_by_key(|path| path.file_name());
    let mut mode = "terminate";
    for path in paths {
        let parent = path.parent().ok_or("service drop-in lacks a directory")?;
        let canonical = std::fs::canonicalize(parent).map_err(|error| error.to_string())?;
        if !roots.contains(&canonical)
            || path.extension().is_none_or(|extension| extension != "conf")
        {
            return Err("unit-specific service overrides are not authorized".into());
        }
        if let Some(value) = validate_contents(&read(
            &canonical.join(path.file_name().ok_or("invalid policy path")?),
        )?)? {
            mode = value;
        }
    }
    Ok(mode)
}

fn validate_contents(bytes: &[u8]) -> Result<Option<&'static str>, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let mut service = false;
    let mut mode = None;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with(['#', ';']))
    {
        if line == "[Service]" {
            service = true;
            continue;
        }
        let value = match line.split_once('=') {
            Some((key, value)) if service && key.trim() == "TimeoutStopFailureMode" => {
                match value.trim() {
                    "terminate" => Some("terminate"),
                    "abort" => Some("abort"),
                    "kill" => Some("kill"),
                    _ => None,
                }
            }
            _ => None,
        };
        if value.is_none() {
            return Err("global service policy overrides unsupported settings; only native TimeoutStopFailureMode defaults are supported".into());
        }
        mode = value;
    }
    Ok(mode)
}

#[cfg(test)]
mod tests {
    #[rstest::rstest]
    #[case(b"[Service]\nTimeoutStopFailureMode=abort\n".as_slice(), true)]
    #[case(b"[Service]\nExecStart=/unrelated\n".as_slice(), false)]
    #[case(b"[Service]\nTimeoutStopFailureMode=abort\\\nExecStart=/unrelated\n".as_slice(), false)]
    #[case(b"[Unit]\nRequires=foreign.service\n".as_slice(), false)]
    fn only_bounded_native_shutdown_defaults_are_accepted(
        #[case] bytes: &[u8],
        #[case] accepted: bool,
    ) {
        assert_eq!(super::validate_contents(bytes).is_ok(), accepted);
    }
}
