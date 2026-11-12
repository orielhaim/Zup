#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    PreparationReserved,
    PreparationRetirementBefore,
    PreparationRetired,
    PreparationDeleteIntentBefore,
    PreparationDeleteIntent,
    PreparationRemoveBefore,
    PreparationRemoveAfter,
    PreparationParentSyncBefore,
    PreparationParentSynced,
    PreparationReadbackBefore,
    PreparationAbsenceObserved,
    PreparationCompactionBefore,
    PreparationCompacted,
    ResolutionIntentPersisted,
    HistoryArchived,
    HistoryMemberRemoved,
    HistoryJournalMemberRemoved,
    HistoryDirectoryRemoved,
    CollectionPrepared,
    CollectionImageWritable,
    CollectionDeleting,
    CollectionMemberRemoved,
    CollectionCompleted,
    InstallationReserved,
    FixedInstanceRetired,
    FixedInstancePublished,
    FixedRollbackDisplaced,
    FixedRollbackRestored,
    PersistentCreationRequested,
    PersistentDirectoryCreated,
    PersistentCreationRecorded,
    MaterializationDirectoryCreated,
    MaterializationStreamInterrupted,
    MaterializationPreparationRecorded,
    MaterializationPublicationRequested,
    MaterializationMemberExposed,
    MaterializationNamespaceExposed,
    MaterializationAccessPublished,
    MaterializationDirectoryPublished,
    IntentBefore,
    IntentPersisted,
    NativeRequestBefore,
    NativeRequestSubmitted,
    NativeConfigurationChanged,
    NativeAdmissionInhibited,
    ApplyReturned,
    ReceiptPersisted,
    VerifyBefore,
    VerifyReturned,
    VerifiedPersisted,
    RollbackIntentPersisted,
    RollbackReturned,
    RolledBackPersisted,
    CommitBefore,
    Committed,
    InstallationPublicationRequested,
    InstallationPublished,
    ReconciliationBefore,
    Reconciled,
}

#[cfg(feature = "test-faults")]
pub fn boundary(point: Boundary, operation: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static MATCHES: AtomicUsize = AtomicUsize::new(0);
    let name = serde_json::to_value(point).expect("a boundary serializes");
    if std::env::var("ZUP_TEST_FAULT_POINT").ok().as_deref() != name.as_str()
        || std::env::var("ZUP_TEST_FAULT_OPERATION")
            .is_ok_and(|filter| !operation.contains(&filter))
    {
        return;
    }
    let occurrence = std::env::var("ZUP_TEST_FAULT_OCCURRENCE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=1024).contains(value))
        .unwrap_or(1);
    if MATCHES.fetch_add(1, Ordering::SeqCst) + 1 == occurrence {
        match std::env::var("ZUP_TEST_FAULT_ACTION").as_deref() {
            Err(_) | Ok("crash") => std::process::abort(),
            Ok("pause") => {
                let latch = std::path::PathBuf::from(
                    std::env::var_os("ZUP_TEST_FAULT_LATCH").expect("pause requires a latch path"),
                );
                std::fs::write(latch.with_extension("ready"), operation)
                    .expect("publish paused operation");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !latch
                    .with_extension("release")
                    .try_exists()
                    .expect("read pause release")
                {
                    assert!(std::time::Instant::now() < deadline, "fault pause expired");
                    std::thread::park_timeout(std::time::Duration::from_millis(20));
                }
            }
            Ok(action) => panic!("unknown fault action {action}"),
        }
    }
}

#[cfg(not(feature = "test-faults"))]
#[inline]
pub fn boundary(_point: Boundary, _operation: &str) {}
