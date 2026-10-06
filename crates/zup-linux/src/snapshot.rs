//! Reading the machine before planning anything for it.
//!
//! The portable delta planner compares desired state against observed state,
//! and this module is the observation. Every file the target plan names is
//! read through the same descriptor-relative, symlink-refusing lookup the
//! executor uses, so planning and execution agree about what is there: a
//! planner that followed links while its executor refused them would plan
//! tranquillity and execute refusals.
//!
//! A symbolic link, a directory, or a device node where a payload file belongs
//! is reported as `NonFile` rather than read through. That flows into the
//! portable planner's conflict handling, which is exactly where a destination
//! substitution belongs: it is a disagreement about the world, not a file with
//! surprising bytes.

use zup_exec::{HostSnapshot, ObservedFile, ObservedFileState};
use zup_platform::TargetPlan;

use crate::fs::{EntryKind, OwnedDirectory};
use crate::lowering::to_host_path;

/// Observe every file a target plan names, in plan order.
///
/// Inspection never mutates: it opens directories read-only and reads files it
/// finds. A destination whose parent does not exist is absent, not an error -
/// an installation into a fresh directory is the common case, not a failure.
pub fn snapshot_target(target: &TargetPlan) -> HostSnapshot {
    let mut snapshot = HostSnapshot::default();
    for file in &target.files {
        let state = observe_file(file);
        snapshot.files.push(ObservedFile {
            key: file.key.clone(),
            path: file.destination.clone(),
            state,
        });
    }
    snapshot
}

/// What occupies one desired file's destination.
fn observe_file(file: &zup_platform::TargetFile) -> ObservedFileState {
    let host = match to_host_path(&file.destination) {
        Ok(host) => host,
        // Unlowerable here means unresolvable, and resolution already proved
        // every destination lowers. Reporting it absent would plan a create
        // over a path the executor cannot spell.
        Err(_) => return ObservedFileState::NonFile,
    };
    let parent = match host
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent,
        None => return ObservedFileState::NonFile,
    };
    let directory = match OwnedDirectory::open(parent) {
        Ok(directory) => directory,
        // No parent directory is the ordinary absent case.
        Err(_) => return ObservedFileState::Absent,
    };
    let name = match host
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    {
        Some(name) if !name.is_empty() => name,
        _ => return ObservedFileState::NonFile,
    };
    match directory.kind_or_absent(&name) {
        Ok(None) => ObservedFileState::Absent,
        Ok(Some(EntryKind::Regular)) => match directory.read_regular(&name) {
            Ok(bytes) => match zup_core::hash_reader(bytes.as_slice()) {
                Ok((size, sha256)) => ObservedFileState::File { size, sha256 },
                Err(_) => ObservedFileState::NonFile,
            },
            Err(_) => ObservedFileState::NonFile,
        },
        // A link, a directory, or a device node where a payload belongs is
        // not a file with surprising bytes. It is a conflict, and the
        // planner - not this observation - decides what that means.
        Ok(Some(_)) => ObservedFileState::NonFile,
        Err(_) => ObservedFileState::NonFile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{AppId, NonEmptyString, Privilege, RelativePath, SelectedScope, TargetTriple};

    fn target_with(files: Vec<(String, zup_platform::TargetPath)>) -> TargetPlan {
        let target = TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target");
        TargetPlan {
            app: zup_core::App {
                id: AppId::new("com.example.tool").expect("an id"),
                name: NonEmptyString::new("Tool").expect("a name"),
                version: semver::Version::parse("1.0.0").expect("a version"),
                publisher: None,
                main: None,
                description: None,
            },
            target: target.clone(),
            scope: SelectedScope::User,
            install_directory: zup_platform::TargetPath::new(target, "/tmp/zup-snapshot-test/tool")
                .expect("a path"),
            selected_components: Vec::new(),
            prerequisites: Vec::new(),
            files: files
                .into_iter()
                .map(|(name, destination)| zup_platform::TargetFile {
                    key: zup_core::ResourceKey::File {
                        destination: destination.to_string(),
                    },
                    source_relative: RelativePath::new(&name).expect("a path"),
                    destination,
                    size: 0,
                    sha256: zup_core::hash_bytes(b""),
                    privilege: Privilege::User,
                    executable: false,
                })
                .collect(),
            launchers: Vec::new(),
            path_entries: Vec::new(),
            services: Vec::new(),
            protocols: Vec::new(),
            file_associations: Vec::new(),
            summary: zup_platform::TargetPlanSummary {
                file_count: 0,
                install_bytes: 0,
                resource_count: 0,
                requires_authorization: false,
                selected_component_count: 0,
                prerequisite_count: 0,
                download_bytes: 0,
            },
            preset: None,
        }
    }

    fn destination(root: &std::path::Path, name: &str) -> zup_platform::TargetPath {
        zup_platform::TargetPath::new(
            TargetTriple::parse("x86_64-unknown-linux-gnu").expect("a target"),
            root.join(name).to_string_lossy(),
        )
        .expect("a path")
    }

    #[test]
    fn absent_stays_absent_and_present_is_identified() {
        let root = tempfile::tempdir().expect("a temp directory");
        std::fs::write(root.path().join("here.dat"), b"the bytes").expect("write");
        let plan = target_with(vec![
            ("here.dat".to_owned(), destination(root.path(), "here.dat")),
            ("gone.dat".to_owned(), destination(root.path(), "gone.dat")),
        ]);

        let snapshot = snapshot_target(&plan);

        assert_eq!(snapshot.files.len(), 2);
        assert!(
            matches!(
                snapshot.files[0].state,
                ObservedFileState::File { size: 9, .. }
            ),
            "{:?}",
            snapshot.files[0].state
        );
        assert_eq!(snapshot.files[1].state, ObservedFileState::Absent);
    }

    /// A link where a payload belongs is not observed through. Planning sees a
    /// non-file, the portable planner calls it a conflict, and no transaction
    /// is compiled - which is the entire point of observing with `O_NOFOLLOW`.
    #[test]
    fn a_symlink_is_a_non_file_not_a_reading() {
        let root = tempfile::tempdir().expect("a temp directory");
        let elsewhere = tempfile::tempdir().expect("an unrelated tree");
        std::fs::write(elsewhere.path().join("target"), b"elsewhere").expect("write");
        std::os::unix::fs::symlink(
            elsewhere.path().join("target"),
            root.path().join("link.dat"),
        )
        .expect("symlink");
        let plan = target_with(vec![(
            "link.dat".to_owned(),
            destination(root.path(), "link.dat"),
        )]);

        let snapshot = snapshot_target(&plan);

        assert_eq!(snapshot.files[0].state, ObservedFileState::NonFile);
    }
}
