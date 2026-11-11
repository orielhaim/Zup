use super::*;
use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
use zup_transaction::{MaterializationRecord, MaterializationState, TransactionStore};

const FILES: [(&str, &[u8], bool); 3] = [
    (
        "bin/tools/run",
        b"#!/bin/sh\nprintf 'generation executable'\n",
        true,
    ),
    ("lib/data.txt", b"nested data", false),
    ("lib/deeper/assets/data.txt", b"deep data", false),
];
const DIRECTORIES: [&str; 6] = [
    "",
    "lib",
    "lib/deeper",
    "lib/deeper/assets",
    "bin",
    "bin/tools",
];

fn effects(root: &Path, source: &Path, scope: SelectedScope) -> DeploymentEffects {
    let mut effects = deployment_effects(root, source, b"nested data", None, None);
    effects.requirements.scope = scope;
    effects.requirements.data.clear();
    effects.data.clear();
    let privilege = scope.authorization();
    let prototype = effects.requirements.files[0].clone();
    effects.requirements.files = FILES
        .iter()
        .map(|(path, contents, executable)| {
            let path = RelativePath::new(*path).unwrap();
            let mut file = prototype.clone();
            file.key = zup_core::ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::TreeMember {
                        declaration: zup_core::ResourceId::new("application").unwrap(),
                        member: path.clone(),
                    },
                },
            };
            file.placement = zup_core::PayloadPlacement::Generation { path };
            file.size = contents.len() as u64;
            file.sha256 = hash_bytes(contents);
            file.executable = *executable;
            file.privilege = privilege;
            file
        })
        .collect();
    let intent = &mut effects.generations[0];
    intent.privilege = privilege;
    intent.sources = effects
        .requirements
        .files
        .iter()
        .zip(FILES)
        .map(|(file, (path, _, _))| {
            let path = RelativePath::new(path).unwrap();
            (
                path.clone(),
                zup_plan::GenerationSource {
                    key: file.key.clone(),
                    source_relative: path,
                },
            )
        })
        .collect();
    intent.materialization = MaterializationSpec::new(
        intent.materialization.installation(),
        intent.materialization.id(),
        intent.materialization.directory().clone(),
        GenerationDescriptor::new(
            Target::LINUX_X64,
            FILES
                .iter()
                .map(|(path, contents, executable)| GenerationFile {
                    path: RelativePath::new(*path).unwrap(),
                    size: contents.len() as u64,
                    sha256: hash_bytes(contents),
                    executable: *executable,
                })
                .collect(),
        )
        .unwrap(),
    )
    .unwrap();
    effects
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[rstest::rstest]
fn nested_generation_access_recovers_at_publication_boundaries(
    #[values(SelectedScope::Machine, SelectedScope::User)] scope: SelectedScope,
    #[values(
        "apply_returned",
        "materialization_publication_requested",
        "materialization_member_exposed",
        "materialization_access_published",
        "materialization_directory_published"
    )]
    point: &str,
) {
    const CHILD_ROOT: &str = "ZUP_ACCESS_PUBLICATION_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = std::path::PathBuf::from(root);
        let source = root.join("source");
        for (path, contents, _) in FILES {
            let path = source.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let state = root.join("state");
        commit_effects(
            &state,
            &source,
            effects(&root.join("application"), &source, scope),
        );
        panic!("fault injection must terminate the child");
    }
    let root = tempfile::tempdir().unwrap();
    let thread = std::thread::current();
    let operation = if point == "apply_returned" {
        "generation:materialize:"
    } else {
        "generation:seal:"
    };
    let child = std::process::Command::new("sh")
        .args([
            "-c",
            "umask 077; ulimit -c 0; exec \"$@\"",
            "zup-access-publication",
        ])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", thread.name().unwrap(), "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .env("ZUP_TEST_FAULT_POINT", point)
        .env("ZUP_TEST_FAULT_OPERATION", operation)
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    assert_eq!(
        child.status.signal(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );

    let state = root.path().join("state");
    let store = FilesystemTransactionStore::private_at(&state);
    let ids = store.list_ids().unwrap();
    assert_eq!(ids.len(), 1);
    let pending = store.load(&ids[0]).unwrap();
    let intent = pending
        .plan
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            zup_plan::NodeKind::Generation { intent, .. } => Some(intent),
            _ => None,
        })
        .unwrap();
    let record_path = state
        .join("materializations")
        .join(intent.materialization.installation().to_string())
        .join(format!("{}.json", intent.materialization.id()));
    let recorded: MaterializationRecord =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(recorded.retired_preparations.len(), 0);
    if point == "apply_returned" {
        assert!(matches!(
            recorded.state,
            MaterializationState::Prepared { .. }
        ));
    } else {
        assert!(matches!(
            recorded.state,
            MaterializationState::Publishing { .. }
        ));
    }
    let destination = Path::new(intent.materialization.directory().as_str());
    let staging = destination
        .parent()
        .unwrap()
        .join(recorded.state.directory().unwrap().as_str());
    let observed = if point == "materialization_directory_published" {
        destination
    } else {
        &staging
    };
    let published_mode = if scope == SelectedScope::Machine {
        0o755
    } else {
        0o700
    };
    let modes: Vec<_> = DIRECTORIES
        .iter()
        .map(|path| mode(&observed.join(path)))
        .collect();
    match point {
        "apply_returned" | "materialization_publication_requested" => {
            assert!(modes.iter().all(|mode| *mode == 0o700))
        }
        "materialization_member_exposed" => {
            assert_eq!(mode(observed), 0o700);
            assert!(
                modes
                    .iter()
                    .all(|mode| *mode == 0o700 || *mode == published_mode)
            );
            if scope == SelectedScope::Machine {
                assert!(modes.contains(&0o755));
                assert!(modes.contains(&0o700));
            }
        }
        _ => assert!(modes.iter().all(|mode| *mode == published_mode)),
    }
    let object = recorded.object;
    let tree = recorded.state.tree().unwrap().clone();
    let mut executor = native_executor(&state, &root.path().join("source"), &pending);
    let publisher = native_publisher(&state);
    let (committed, outcome) =
        zup_transaction::recover(pending, &store, &mut executor, &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed, "{committed:?}");
    let installed = publisher.load(&committed.app_id, scope).unwrap().unwrap();
    let selected = installed.current.materialization.as_ref().unwrap();
    let published = Path::new(selected.directory().as_str());
    for path in DIRECTORIES {
        assert_eq!(mode(&published.join(path)), published_mode);
    }
    for (path, contents, executable) in FILES {
        assert_eq!(std::fs::read(published.join(path)).unwrap(), contents);
        assert_eq!(
            mode(&published.join(path)),
            if executable { 0o555 } else { 0o444 }
        );
    }
    let output = std::process::Command::new(published.join("bin/tools/run"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"generation executable");
    let sealed: MaterializationRecord =
        serde_json::from_slice(&std::fs::read(record_path).unwrap()).unwrap();
    assert_eq!(sealed.object, object);
    assert_eq!(sealed.state, MaterializationState::Sealed { tree });
    assert!(sealed.retired_preparations.is_empty());
}
