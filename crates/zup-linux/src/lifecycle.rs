use zup_exec::{InstallLedger, LifecycleAction};
use zup_runtime::{InstallOutcome, Operation, OperationError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxOutcome {
    Committed { version: semver::Version },
    RolledBack,
    RecoveryRequired { transaction: String },

    Busy,
}

impl LinuxOutcome {
    pub fn as_install_outcome(&self) -> InstallOutcome {
        match self {
            Self::Committed { .. } => InstallOutcome::Committed,
            Self::RolledBack => InstallOutcome::RolledBack,
            Self::RecoveryRequired { .. } => InstallOutcome::RecoveryRequired,
            Self::Busy => InstallOutcome::Busy {
                operation: "linux install",
            },
        }
    }
}

pub fn resolve_action(
    operation: Operation,
    ledger: Option<&InstallLedger>,
    version: &semver::Version,
) -> Result<LifecycleAction, OperationError> {
    zup_runtime::resolve_operation(operation, ledger.map(|ledger| &ledger.version), version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(version: &str) -> InstallLedger {
        let mut ledger = InstallLedger::new(
            zup_core::AppId::new("com.example.tool").expect("valid app id"),
            zup_core::Target::parse("linux-x64").expect("valid target"),
            zup_core::SelectedScope::User,
        );
        ledger.version = semver::Version::parse(version).expect("valid version");
        ledger
    }

    #[test]
    fn apply_without_a_ledger_is_an_install() {
        assert_eq!(
            resolve_action(Operation::Apply, None, &semver::Version::new(1, 0, 0)).unwrap(),
            LifecycleAction::Install,
        );
    }

    #[test]
    fn apply_compares_versions_rather_than_trusting_the_caller() {
        let installed = ledger("1.0.0");
        assert_eq!(
            resolve_action(
                Operation::Apply,
                Some(&installed),
                &semver::Version::new(2, 0, 0)
            )
            .unwrap(),
            LifecycleAction::Upgrade,
        );
        assert_eq!(
            resolve_action(
                Operation::Apply,
                Some(&installed),
                &semver::Version::new(1, 0, 0)
            )
            .unwrap(),
            LifecycleAction::Modify,
        );
        assert!(
            resolve_action(
                Operation::Apply,
                Some(&installed),
                &semver::Version::new(0, 9, 0)
            )
            .is_err(),
            "a downgrade is refused rather than applied",
        );
    }

    #[test]
    fn every_outcome_maps_to_the_session_vocabulary() {
        assert_eq!(
            LinuxOutcome::Committed {
                version: semver::Version::new(1, 0, 0)
            }
            .as_install_outcome(),
            InstallOutcome::Committed,
        );
        assert_eq!(
            LinuxOutcome::RolledBack.as_install_outcome(),
            InstallOutcome::RolledBack,
        );
        assert_eq!(
            LinuxOutcome::RecoveryRequired {
                transaction: "id".to_owned()
            }
            .as_install_outcome(),
            InstallOutcome::RecoveryRequired,
        );
        assert!(matches!(
            LinuxOutcome::Busy.as_install_outcome(),
            InstallOutcome::Busy { .. },
        ));
    }
}
