use zup_runtime::InstallOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxOutcome {
    Committed { version: semver::Version },
    RolledBack,
    RecoveryRequired { transaction: String },
    CommittedPendingReconciliation { transaction: String },
    Busy,
}

impl LinuxOutcome {
    pub fn as_install_outcome(&self) -> InstallOutcome {
        match self {
            Self::Committed { .. } => InstallOutcome::Committed,
            Self::RolledBack => InstallOutcome::RolledBack,
            Self::RecoveryRequired { .. } => InstallOutcome::RecoveryRequired,
            Self::CommittedPendingReconciliation { .. } => {
                InstallOutcome::CommittedPendingReconciliation {
                    detail: "the transaction committed and ledger reconciliation is still required"
                        .into(),
                }
            }
            Self::Busy => InstallOutcome::Busy {
                operation: "linux install",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
