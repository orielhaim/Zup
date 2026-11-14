use super::*;
use std::os::unix::fs::PermissionsExt;

const MAINTENANCE: &[u8] = b"#!/bin/sh\nprintf 'maintenance executable'\n";

fn consumer(script: &str, paths: &[&Path]) -> std::process::Output {
    std::process::Command::new("setpriv")
        .args([
            "--reuid=65534", "--regid=65534", "--clear-groups",
            "--bounding-set=-all", "--inh-caps=-all", "--ambient-caps=-all",
            "--no-new-privs", "/bin/sh", "-eu", "-c",
        ])
        .arg(format!(
            "test \"$(id -u)\" = 65534; test \"$(id -G)\" = 65534; \
             test \"$(grep -Ec '^Cap(Inh|Prm|Eff|Bnd|Amb):[[:space:]]*0000000000000000$' /proc/self/status)\" = 5; \
             deny() {{ if \"$@\"; then echo 'unexpected consumer authority' >&2; exit 1; fi; }}; {script}"
        ))
        .arg("zup-consumer")
        .args(paths)
        .output()
        .expect("privileged Linux tests require util-linux setpriv")
}

pub(super) fn assert_consumer(script: &str, paths: &[&Path]) {
    let output = consumer(script, paths);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn with_maintenance(mut effects: DeploymentEffects, id: MaterializationId) -> DeploymentEffects {
    let mut maintenance = effects.generations[0].clone();
    let artifact = zup_core::MaintenanceArtifact::Runtime;
    let path = artifact.member_path(Target::LINUX_X64, hash_bytes(MAINTENANCE));
    let key = zup_core::ResourceKey::File {
        id: zup_core::FileResourceId::Maintenance { artifact },
    };
    maintenance.sources = BTreeMap::from([(
        path.clone(),
        zup_plan::GenerationSource {
            key: key.clone(),
            source_relative: path.clone(),
        },
    )]);
    effects.requirements.files.push(zup_plan::RequiredFile {
        key,
        placement: zup_core::PayloadPlacement::Generation { path: path.clone() },
        size: MAINTENANCE.len() as u64,
        sha256: hash_bytes(MAINTENANCE),
        executable: true,
        privilege: maintenance.privilege,
    });
    let files = vec![GenerationFile {
        path,
        size: MAINTENANCE.len() as u64,
        sha256: hash_bytes(MAINTENANCE),
        executable: true,
    }];
    maintenance.materialization = MaterializationSpec::new(
        effects.installation,
        id,
        effects
            .install_directory
            .as_ref()
            .unwrap()
            .join(format!(
                "{}/{id}",
                zup_core::MAINTENANCE_DEPLOYMENTS_DIRECTORY
            ))
            .unwrap(),
        GenerationDescriptor::new(Target::LINUX_X64, files).unwrap(),
    )
    .unwrap();
    maintenance.publication = zup_plan::GenerationPublication::Maintenance;
    effects.generations.push(maintenance);
    effects
}

fn fixture_effects(
    root: &Path,
    scope: SelectedScope,
    maintenance: MaterializationId,
) -> DeploymentEffects {
    let source = root.join("source");
    for (path, contents, _) in super::access_publication::FILES {
        let path = source.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    std::fs::write(source.join("maintenance"), MAINTENANCE).unwrap();
    with_maintenance(
        super::access_publication::effects(&root.join("application"), &source, scope),
        maintenance,
    )
}

#[rstest::rstest]
#[ignore = "requires root and util-linux setpriv; runs consumers as uid 65534"]
fn published_content_is_accessible_without_namespace_write_authority(
    #[values(SelectedScope::Machine, SelectedScope::User)] scope: SelectedScope,
    #[values(false, true)] reuse_namespace: bool,
) {
    assert_eq!(
        rustix::process::geteuid().as_raw(),
        0,
        "run this test as root"
    );
    let fixture = tempfile::tempdir().unwrap();
    std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    let state = fixture.path().join("state");
    let application = fixture.path().join("application");
    let source = fixture.path().join("source");
    if reuse_namespace {
        for path in [
            application.clone(),
            application.join(".zup"),
            application.join(".zup/deployments"),
            application.join(".zup/maintenance"),
        ] {
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let effects = fixture_effects(fixture.path(), scope, MaterializationId::new_v7());
    let intents = effects.generations.clone();
    commit_effects(&state, &source, effects);
    let app = Path::new(intents[0].materialization.directory().as_str());
    let maintenance = Path::new(intents[1].materialization.directory().as_str());
    let metadata = state
        .join("materializations")
        .join(intents[0].materialization.installation().to_string())
        .join(format!("{}.json", intents[0].materialization.id()));
    let staging = application.join(".zup/deployments/.preparing-private");
    std::fs::create_dir(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(staging.join("secret"), b"private staging").unwrap();
    if scope == SelectedScope::Machine {
        assert_consumer(
            "test \"$(cat \"$1/lib/data.txt\")\" = 'nested data'; test \"$(\"$1/bin/tools/run\")\" = 'generation executable'; test \"$(\"$2/maintenance\")\" = 'maintenance executable'; grep -q 'maintenance executable' \"$2/maintenance\"",
            &[app, maintenance],
        );
    } else {
        assert_consumer(
            "deny cat \"$1/lib/data.txt\"; deny \"$1/bin/tools/run\"; deny \"$2/maintenance\"",
            &[app, maintenance],
        );
    }
    assert_consumer(
        "deny touch \"$1/new\"; deny mv \"$2\" \"$1/replaced\"; deny sh -c 'echo changed > \"$1/lib/data.txt\"' sh \"$2\"; deny rm \"$2/lib/data.txt\"; deny cat \"$3\"; deny cat \"$4/secret\"; deny ls \"$1\"; deny sh -c 'cd \"$1\"' sh \"$4\"",
        &[app.parent().unwrap(), app, &metadata, &staging],
    );
    assert_consumer(
        "deny touch \"$1/new\"; deny mv \"$2\" \"$1/replaced\"; deny sh -c 'echo changed > \"$1/maintenance\"' sh \"$2\"; deny rm \"$2/maintenance\"",
        &[maintenance.parent().unwrap(), maintenance],
    );
}

#[rstest::rstest]
#[ignore = "requires root and util-linux setpriv; runs consumers as uid 65534"]
fn consumers_reach_both_generations_after_interrupted_namespace_publication(
    #[values(
        "materialization_namespace_exposed",
        "materialization_access_published"
    )]
    point: &str,
) {
    use std::os::unix::process::ExitStatusExt;
    use zup_transaction::{MaterializationRecord, MaterializationState, TransactionStore};
    const CHILD_ROOT: &str = "ZUP_NAMESPACE_PUBLICATION_ROOT";
    assert_eq!(
        rustix::process::geteuid().as_raw(),
        0,
        "run this test as root"
    );
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = std::path::PathBuf::from(root);
        let id =
            serde_json::from_str(&std::env::var("ZUP_NAMESPACE_MAINTENANCE_ID").unwrap()).unwrap();
        let effects = fixture_effects(&root, SelectedScope::Machine, id);
        commit_effects(&root.join("state"), &root.join("source"), effects);
        panic!("publication fault must terminate the child");
    }
    let fixture = tempfile::tempdir().unwrap();
    let maintenance_id = MaterializationId::new_v7();
    std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    let child = std::process::Command::new("sh")
        .args([
            "-c",
            "umask 077; ulimit -c 0; exec \"$@\"",
            "zup-namespace-publication",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            std::thread::current().name().unwrap(),
            "--nocapture",
        ])
        .env(CHILD_ROOT, fixture.path())
        .env("ZUP_TEST_FAULT_POINT", point)
        .env(
            "ZUP_TEST_FAULT_OPERATION",
            format!("generation:seal:{maintenance_id}"),
        )
        .env(
            "ZUP_NAMESPACE_MAINTENANCE_ID",
            serde_json::to_string(&maintenance_id).unwrap(),
        )
        .env("ZUP_TEST_FAULT_ACTION", "crash")
        .output()
        .unwrap();
    assert_eq!(
        child.status.signal(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let state = fixture.path().join("state");
    let source = fixture.path().join("source");
    let store = FilesystemTransactionStore::private_at(&state);
    let pending = store.load(&store.list_ids().unwrap()[0]).unwrap();
    let intent = pending
        .plan
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            zup_plan::NodeKind::Generation { intent, .. }
                if intent.publication == zup_plan::GenerationPublication::Maintenance =>
            {
                Some(intent.as_ref())
            }
            _ => None,
        })
        .unwrap();
    let metadata = state
        .join("materializations")
        .join(intent.materialization.installation().to_string())
        .join(format!("{}.json", intent.materialization.id()));
    let record: MaterializationRecord =
        serde_json::from_slice(&std::fs::read(&metadata).unwrap()).unwrap();
    assert!(matches!(
        record.state,
        MaterializationState::Publishing { .. }
    ));
    let namespace = Path::new(intent.materialization.directory().as_str())
        .parent()
        .unwrap()
        .to_owned();
    let staging = namespace.join(record.state.directory().unwrap().as_str());
    if point == "materialization_namespace_exposed" {
        assert_consumer(
            "deny sh -c 'cd \"$1\"' sh \"$1\"; deny cat \"$1/maintenance\"; deny cat \"$2\"",
            &[&staging, &metadata],
        );
        // Model a crash between individual ancestor updates as well.
        std::fs::set_permissions(
            namespace.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let mut executor = native_executor(&state, &source, &pending);
    let publisher = native_publisher(&state);
    let (committed, outcome) =
        zup_transaction::recover(pending, &store, &mut executor, &publisher).unwrap();
    assert_eq!(outcome, TransactionOutcome::Committed, "{committed:?}");
    let installed = publisher
        .load(&committed.app_id, SelectedScope::Machine)
        .unwrap()
        .unwrap();
    let app = Path::new(
        installed
            .current
            .materialization
            .as_ref()
            .unwrap()
            .directory()
            .as_str(),
    );
    let maintenance = Path::new(
        installed
            .current
            .maintenance
            .as_ref()
            .unwrap()
            .directory()
            .as_str(),
    );
    assert_consumer(
        "test \"$(cat \"$1/lib/data.txt\")\" = 'nested data'; test \"$(\"$1/bin/tools/run\")\" = 'generation executable'; test \"$(\"$2/maintenance\")\" = 'maintenance executable'",
        &[app, maintenance],
    );
    assert_consumer(
        "deny cat \"$1\"; deny touch \"$2/new\"",
        &[&metadata, &namespace],
    );
    assert_eq!(
        std::fs::metadata(&namespace).unwrap().permissions().mode() & 0o7777,
        0o711
    );
}
