#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    IntentBefore,
    IntentPersisted,
    NativeRequestBefore,
    NativeRequestSubmitted,
    NativeConfigurationChanged,
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
    LedgerPublishBefore,
    LedgerPublished,
}

#[cfg(feature = "test-faults")]
pub fn boundary(point: Boundary, operation: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static MATCHES: AtomicUsize = AtomicUsize::new(0);
    let name = serde_json::to_value(point).expect("a boundary serializes");
    if std::env::var("ZUP_TEST_CRASH_POINT").ok().as_deref() != name.as_str()
        || std::env::var("ZUP_TEST_CRASH_OPERATION")
            .is_ok_and(|filter| !operation.contains(&filter))
    {
        return;
    }
    let occurrence = std::env::var("ZUP_TEST_CRASH_OCCURRENCE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=1024).contains(value))
        .unwrap_or(1);
    if MATCHES.fetch_add(1, Ordering::SeqCst) + 1 == occurrence {
        std::process::abort();
    }
}

#[cfg(not(feature = "test-faults"))]
#[inline]
pub fn boundary(_point: Boundary, _operation: &str) {}
