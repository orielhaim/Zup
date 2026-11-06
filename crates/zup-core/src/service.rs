use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WindowsServiceAccount {
    LocalService,
    NetworkService,
    LocalSystem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceExecution {
    Windows {
        account: WindowsServiceAccount,
    },
    SystemdDynamicUser,
    OpenrcUser {
        user: String,
        #[serde(default)]
        group: Option<String>,
    },
    SystemdUser {
        user: String,
        #[serde(default)]
        group: Option<String>,
    },
}

impl ServiceExecution {
    pub fn validate_policies(
        &self,
        failure: ServiceFailurePolicy,
        readiness: ServiceReadiness,
        target: crate::Target,
    ) -> Result<(), &'static str> {
        self.validate_for(target)?;
        failure.validate_for(target)?;
        readiness.validate_for(target)?;
        let supported = matches!(
            (self, failure),
            (_, ServiceFailurePolicy::None)
                | (Self::Windows { .. }, ServiceFailurePolicy::Windows { .. })
                | (
                    Self::SystemdUser { .. } | Self::SystemdDynamicUser,
                    ServiceFailurePolicy::Systemd { .. }
                )
                | (Self::OpenrcUser { .. }, ServiceFailurePolicy::Openrc { .. })
        );
        if !supported {
            return Err("runtime failure policy does not match the service manager");
        }
        if readiness == ServiceReadiness::Notify && matches!(self, Self::OpenrcUser { .. }) {
            return Err("systemd notification is not an OpenRC readiness protocol");
        }
        Ok(())
    }

    pub fn validate_for(&self, target: crate::Target) -> Result<(), &'static str> {
        match (self, target.operating_system()) {
            (Self::Windows { .. }, crate::TargetOperatingSystem::Windows)
            | (Self::SystemdDynamicUser, crate::TargetOperatingSystem::Linux) => Ok(()),
            (
                Self::SystemdUser { user, group } | Self::OpenrcUser { user, group },
                crate::TargetOperatingSystem::Linux,
            ) => {
                let valid = |name: &str| {
                    !name.is_empty()
                        && name.len() <= 31
                        && name
                            .bytes()
                            .next()
                            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                };
                if valid(user) && group.as_deref().is_none_or(valid) {
                    Ok(())
                } else {
                    Err(
                        "service user and group must be plain system account names of at most 31 bytes",
                    )
                }
            }
            _ => Err("service execution identity does not match the target service manager"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ServiceReadiness {
    #[default]
    Manager,
    Notify,
}

impl ServiceReadiness {
    pub fn validate_for(self, target: crate::Target) -> Result<(), &'static str> {
        if self == Self::Notify && target.operating_system() != crate::TargetOperatingSystem::Linux
        {
            return Err("notify readiness requires the Linux systemd backend");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum InitialServiceState {
    #[default]
    Stopped,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChangedServiceState {
    #[default]
    Preserve,
    Stopped,
}

/// Boot policy remains independent of these deployment-time decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ServiceLifecycle {
    #[serde(default)]
    pub install: InitialServiceState,
    #[serde(default)]
    pub change: ChangedServiceState,
    #[serde(default = "default_timeout")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
    pub start_timeout_seconds: u32,
    #[serde(default = "default_timeout")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
    pub stop_timeout_seconds: u32,
}

const fn default_timeout() -> u32 {
    90
}

impl Default for ServiceLifecycle {
    fn default() -> Self {
        Self {
            install: InitialServiceState::Stopped,
            change: ChangedServiceState::Preserve,
            start_timeout_seconds: default_timeout(),
            stop_timeout_seconds: default_timeout(),
        }
    }
}

impl ServiceLifecycle {
    pub fn validate(self, start: crate::ServiceStart) -> Result<(), &'static str> {
        if !(1..=3600).contains(&self.start_timeout_seconds)
            || !(1..=3600).contains(&self.stop_timeout_seconds)
        {
            return Err("service transition timeouts must be between 1 and 3600 seconds");
        }
        if start == crate::ServiceStart::Disabled && self.install == InitialServiceState::Running {
            return Err("disabled services cannot request startup after installation");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceFailurePolicy {
    #[default]
    None,
    Systemd {
        condition: SystemdRestartCondition,
        delay_seconds: u32,
        start_limit_burst: u32,
        start_limit_interval_seconds: u32,
    },
    Windows {
        delay_seconds: u32,
        restart_attempts: u32,
        reset_seconds: u32,
    },
    Openrc {
        delay_seconds: u32,
        max_respawns: u32,
        respawn_period_seconds: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SystemdRestartCondition {
    OnFailure,
    OnAbnormal,
}

impl ServiceFailurePolicy {
    pub fn validate_for(self, target: crate::Target) -> Result<(), &'static str> {
        let bounded = |delay, attempts, interval| {
            (1..=3600).contains(&delay)
                && (1..=100).contains(&attempts)
                && (1..=86400).contains(&interval)
                && delay * attempts < interval
        };
        match self {
            Self::None => Ok(()),
            Self::Systemd {
                delay_seconds,
                start_limit_burst,
                start_limit_interval_seconds,
                ..
            } if target.operating_system() == crate::TargetOperatingSystem::Linux
                && bounded(
                    delay_seconds,
                    start_limit_burst,
                    start_limit_interval_seconds,
                ) =>
            {
                Ok(())
            }
            Self::Windows {
                delay_seconds,
                restart_attempts,
                reset_seconds,
            } if target.operating_system() == crate::TargetOperatingSystem::Windows
                && bounded(delay_seconds, restart_attempts, reset_seconds) =>
            {
                Ok(())
            }
            Self::Openrc {
                delay_seconds,
                max_respawns,
                respawn_period_seconds,
            } if target.operating_system() == crate::TargetOperatingSystem::Linux
                && bounded(delay_seconds, max_respawns, respawn_period_seconds) =>
            {
                Ok(())
            }
            _ => Err(
                "runtime failure policy requires the matching native manager, 1-100 retries, a 1-3600 second delay and a 1-86400 second interval longer than delay times retries",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::root("root", true)]
    #[case::named("app-worker", true)]
    #[case::numeric("1000", false)]
    #[case::specifier("%i", false)]
    #[case::directive("app\nUser=root", false)]
    #[case::empty("", false)]
    fn system_accounts_are_literal_names(#[case] user: &str, #[case] valid: bool) {
        let target = crate::Target::parse("linux-x64").unwrap();
        for execution in [
            ServiceExecution::SystemdUser {
                user: user.into(),
                group: None,
            },
            ServiceExecution::SystemdUser {
                user: "worker".into(),
                group: Some(user.into()),
            },
        ] {
            assert_eq!(execution.validate_for(target).is_ok(), valid);
        }
    }

    #[test]
    fn execution_identity_cannot_cross_platforms() {
        let linux = crate::Target::parse("linux-arm64-musl").unwrap();
        let windows = crate::Target::parse("windows-x64").unwrap();
        assert!(
            ServiceExecution::SystemdDynamicUser
                .validate_for(windows)
                .is_err()
        );
        assert!(
            ServiceExecution::Windows {
                account: WindowsServiceAccount::LocalSystem
            }
            .validate_for(linux)
            .is_err()
        );
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[rstest::rstest]
    #[case(1, 3, 60, true)]
    #[case(0, 3, 60, false)]
    #[case(5, 3, 15, false)]
    #[case(1, 0, 60, false)]
    #[case(1, 101, 600, false)]
    fn restart_budget_cannot_allow_an_unbounded_loop(
        #[case] delay: u32,
        #[case] attempts: u32,
        #[case] interval: u32,
        #[case] valid: bool,
    ) {
        for policy in [
            ServiceFailurePolicy::Systemd {
                condition: SystemdRestartCondition::OnFailure,
                delay_seconds: delay,
                start_limit_burst: attempts,
                start_limit_interval_seconds: interval,
            },
            ServiceFailurePolicy::Openrc {
                delay_seconds: delay,
                max_respawns: attempts,
                respawn_period_seconds: interval,
            },
        ] {
            assert_eq!(
                policy
                    .validate_for(crate::Target::parse("linux-x64-musl").unwrap())
                    .is_ok(),
                valid
            );
        }
        let policy = ServiceFailurePolicy::Windows {
            delay_seconds: delay,
            restart_attempts: attempts,
            reset_seconds: interval,
        };
        assert_eq!(
            policy
                .validate_for(crate::Target::parse("windows-x64").unwrap())
                .is_ok(),
            valid
        );
        assert!(
            policy
                .validate_for(crate::Target::parse("linux-x64").unwrap())
                .is_err()
        );
    }
}
