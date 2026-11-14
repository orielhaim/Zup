mod common;

use std::time::Instant;
use zup_transaction::{FilesystemTransactionStore, TransactionCoordinator, TransactionStore};

/// Run in release mode with --ignored --nocapture. Each size gets a fresh native directory.
#[test]
#[ignore = "journal scaling benchmark"]
fn journal_scaling() {
    let sizes = std::env::var("ZUP_BENCH_SIZES").unwrap_or_else(|_| "1,16,128,512".into());
    for count in sizes.split(',').map(|n| n.parse::<usize>().unwrap()) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = FilesystemTransactionStore::private_at(&root);
        let mut input = common::effects();
        input.files = (0..count)
            .map(|i| {
                common::file(
                    &format!("directory-{i:06}/{}-{i}.dat", "long-name-".repeat(12)),
                    b"x",
                )
            })
            .collect();
        let plan = zup_plan::compile_deployment(&input).unwrap();
        let nodes = plan.nodes.len();
        let started = Instant::now();
        let settled = TransactionCoordinator::new(store.clone())
            .begin(
                common::sample_app_id(),
                zup_core::SelectedScope::User,
                common::sample_version(),
                plan,
            )
            .unwrap()
            .execute(
                &mut common::fake::FakeExecutor::new(),
                &common::Publisher::default(),
            )
            .unwrap();
        assert_eq!(
            settled.outcome,
            zup_transaction::TransactionOutcome::Committed
        );
        let elapsed = started.elapsed().as_millis();
        let io = store.io_stats();
        let cold = FilesystemTransactionStore::private_at(&root);
        let started = Instant::now();
        assert_eq!(
            cold.load(&settled.record.transaction_id).unwrap(),
            settled.record
        );
        let recovery_us = started.elapsed().as_micros();
        let disk_bytes: u64 = std::fs::read_dir(
            root.join("transactions")
                .join(settled.record.transaction_id.to_string()),
        )
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
        println!(
            "{}",
            serde_json::json!({"files": count, "nodes": nodes, "revision": settled.record.revision,
            "io": io, "wall_ms": elapsed, "cold_recovery_us": recovery_us, "disk_bytes": disk_bytes})
        );
    }
}

#[test]
#[ignore = "journal compaction benchmark"]
fn journal_repeated_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = FilesystemTransactionStore::private_at(&root);
    let mut record = TransactionCoordinator::new(store.clone())
        .begin(
            common::sample_app_id(),
            zup_core::SelectedScope::User,
            common::sample_version(),
            common::sample_plan(),
        )
        .unwrap()
        .execute(
            &mut common::fake::FakeExecutor::new(),
            &common::Publisher::default(),
        )
        .unwrap()
        .record;
    let started = Instant::now();
    for round in 0..4 {
        for i in 0..256 {
            record = store
                .transition(
                    &record.transaction_id,
                    record.revision,
                    &[zup_transaction::TransactionChange::RecoveryFailure(Some(
                        zup_transaction::TransactionFailure {
                            operation: None,
                            message: format!(
                                "attempt {}: {}",
                                round * 256 + i,
                                "retry evidence ".repeat(600)
                            ),
                        },
                    ))],
                )
                .unwrap();
        }
        let cold = FilesystemTransactionStore::private_at(&root);
        let recovery = Instant::now();
        assert_eq!(cold.load(&record.transaction_id).unwrap(), record);
        let cold_recovery_us = recovery.elapsed().as_micros();
        let journal = root
            .join("transactions")
            .join(record.transaction_id.to_string());
        let disk_bytes: u64 = std::fs::read_dir(&journal)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(disk_bytes < 512 * 1024);
        println!(
            "{}",
            serde_json::json!({"recovery_attempts": (round + 1) * 256, "disk_bytes": disk_bytes,
            "wall_ms": started.elapsed().as_millis(), "cold_recovery_us": cold_recovery_us, "io": store.io_stats()})
        );
    }
}
