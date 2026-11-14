use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOwnershipClaim {
    pub installation: crate::InstallationId,
    pub service: crate::ServiceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeServiceRegistration {
    Scm { installation: crate::InstallationId },
    SourceFile,
}

impl NativeServiceRegistration {
    pub fn for_service(execution: &ServiceExecution, installation: crate::InstallationId) -> Self {
        match execution {
            ServiceExecution::WindowsJob { .. } => Self::Scm { installation },
            _ => Self::SourceFile,
        }
    }

    pub fn claim(self, service: crate::ServiceId) -> Option<ServiceOwnershipClaim> {
        match self {
            Self::Scm { installation } => Some(ServiceOwnershipClaim {
                installation,
                service,
            }),
            Self::SourceFile => None,
        }
    }

    pub fn belongs_to(self, installation: crate::InstallationId, target: crate::Target) -> bool {
        match self {
            Self::Scm {
                installation: owner,
            } => owner == installation && target.is_windows(),
            Self::SourceFile => target.operating_system() == crate::TargetOperatingSystem::Linux,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservedServiceRegistration {
    Unclaimed,
    Scm { claim: ServiceOwnershipClaim },
    SourceFile,
}

impl ObservedServiceRegistration {
    pub fn matches(&self, expected: NativeServiceRegistration, service: &crate::ServiceId) -> bool {
        match (self, expected) {
            (Self::Scm { claim }, NativeServiceRegistration::Scm { installation }) => {
                claim.installation == installation && &claim.service == service
            }
            (Self::SourceFile, NativeServiceRegistration::SourceFile) => true,
            _ => false,
        }
    }
}

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
    WindowsJob {
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
    pub fn native_name(
        &self,
        app: &crate::AppId,
        id: &crate::ServiceId,
        declared: Option<&crate::NonEmptyString>,
    ) -> Result<crate::NonEmptyString, &'static str> {
        let name = match declared {
            Some(name) => name.to_string(),
            None => {
                let mut preimage = b"zup:native-service-name:1\0".to_vec();
                preimage
                    .extend(serde_json::to_vec(&(app, id)).expect("service identity serializes"));
                let digest = crate::hash_bytes(&preimage);
                let suffix = if matches!(self, Self::SystemdUser { .. } | Self::SystemdDynamicUser)
                {
                    ".service"
                } else {
                    ""
                };
                format!("zup-{}{suffix}", digest.to_hex())
            }
        };
        let valid = if matches!(self, Self::WindowsJob { .. }) {
            name.encode_utf16().count() <= 256
                && !name
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        } else {
            let stem = if matches!(self, Self::SystemdUser { .. } | Self::SystemdDynamicUser) {
                name.strip_suffix(".service")
                    .ok_or("systemd native names must end in .service")?
            } else {
                &name
            };
            !stem.is_empty()
                && name.len() <= 200
                && stem
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
                && stem != "."
                && stem != ".."
        };
        if !valid {
            return Err("native service name is not a safe name for the selected service manager");
        }
        crate::NonEmptyString::new(name).map_err(|_| "native service name must not be empty")
    }

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
                | (
                    Self::WindowsJob { .. },
                    ServiceFailurePolicy::Windows { .. }
                )
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
            (Self::WindowsJob { .. }, crate::TargetOperatingSystem::Windows)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ServiceLifecycle {
    #[serde(default)]
    pub install: InitialServiceState,
    #[serde(default)]
    pub change: ChangedServiceState,
    #[serde(default)]
    pub timeouts: Option<ServiceTimeouts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceTimeouts {
    Systemd {
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        start_timeout_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        stop_grace_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        abort_timeout_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        settle_seconds: u32,
    },
    Openrc {
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        start_timeout_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        stop_grace_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        kill_wait_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        settle_seconds: u32,
    },
    Windows {
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        start_observation_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        stop_observation_seconds: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeServiceTimeouts {
    Systemd {
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        start_timeout_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        stop_grace_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        abort_timeout_seconds: u32,
    },
    Openrc {
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        stop_grace_seconds: u32,
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 3600)))]
        kill_wait_seconds: u32,
    },
    Scm,
}

impl ServiceTimeouts {
    pub const fn for_execution(execution: &ServiceExecution) -> Self {
        match execution {
            ServiceExecution::WindowsJob { .. } => Self::Windows {
                start_observation_seconds: 90,
                stop_observation_seconds: 90,
            },
            ServiceExecution::OpenrcUser { .. } => Self::Openrc {
                start_timeout_seconds: 90,
                stop_grace_seconds: 90,
                kill_wait_seconds: 5,
                settle_seconds: 5,
            },
            ServiceExecution::SystemdUser { .. } | ServiceExecution::SystemdDynamicUser => {
                Self::Systemd {
                    start_timeout_seconds: 90,
                    stop_grace_seconds: 90,
                    abort_timeout_seconds: 5,
                    settle_seconds: 5,
                }
            }
        }
    }

    pub const fn native(self) -> NativeServiceTimeouts {
        match self {
            Self::Systemd {
                start_timeout_seconds,
                stop_grace_seconds,
                abort_timeout_seconds,
                ..
            } => NativeServiceTimeouts::Systemd {
                start_timeout_seconds,
                stop_grace_seconds,
                abort_timeout_seconds,
            },
            Self::Openrc {
                stop_grace_seconds,
                kill_wait_seconds,
                ..
            } => NativeServiceTimeouts::Openrc {
                stop_grace_seconds,
                kill_wait_seconds,
            },
            Self::Windows { .. } => NativeServiceTimeouts::Scm,
        }
    }

    pub const fn start_observation_seconds(self) -> u32 {
        match self {
            Self::Systemd {
                start_timeout_seconds,
                settle_seconds,
                ..
            }
            | Self::Openrc {
                start_timeout_seconds,
                settle_seconds,
                ..
            } => start_timeout_seconds.saturating_add(settle_seconds),
            Self::Windows {
                start_observation_seconds,
                ..
            } => start_observation_seconds,
        }
    }

    pub const fn stop_observation_seconds(self) -> u32 {
        match self {
            Self::Systemd {
                stop_grace_seconds,
                abort_timeout_seconds,
                settle_seconds,
                ..
            } => stop_grace_seconds
                .saturating_add(abort_timeout_seconds)
                .saturating_add(settle_seconds),
            Self::Openrc {
                stop_grace_seconds,
                kill_wait_seconds,
                settle_seconds,
                ..
            } => stop_grace_seconds
                .saturating_add(kill_wait_seconds)
                .saturating_add(settle_seconds),
            Self::Windows {
                stop_observation_seconds,
                ..
            } => stop_observation_seconds,
        }
    }

    pub fn validate_for(self, execution: &ServiceExecution) -> Result<(), &'static str> {
        let values: &[u32] = match &self {
            Self::Systemd {
                start_timeout_seconds,
                stop_grace_seconds,
                abort_timeout_seconds,
                settle_seconds,
            } if matches!(
                execution,
                ServiceExecution::SystemdUser { .. } | ServiceExecution::SystemdDynamicUser
            ) =>
            {
                &[
                    *start_timeout_seconds,
                    *stop_grace_seconds,
                    *abort_timeout_seconds,
                    *settle_seconds,
                ]
            }
            Self::Openrc {
                start_timeout_seconds,
                stop_grace_seconds,
                kill_wait_seconds,
                settle_seconds,
            } if matches!(execution, ServiceExecution::OpenrcUser { .. }) => &[
                *start_timeout_seconds,
                *stop_grace_seconds,
                *kill_wait_seconds,
                *settle_seconds,
            ],
            Self::Windows {
                start_observation_seconds,
                stop_observation_seconds,
            } if matches!(execution, ServiceExecution::WindowsJob { .. }) => {
                &[*start_observation_seconds, *stop_observation_seconds]
            }
            _ => return Err("service timeout policy does not match its native manager"),
        };
        if values.iter().all(|seconds| (1..=3600).contains(seconds))
            && self.stop_observation_seconds() <= 3600
            && self.start_observation_seconds() <= 3600
        {
            Ok(())
        } else {
            Err(
                "service timeout components must be positive and derived transition waits must not exceed 3600 seconds",
            )
        }
    }
}

impl NativeServiceTimeouts {
    pub fn validate_for(self, execution: &ServiceExecution) -> Result<(), &'static str> {
        let policy = match self {
            Self::Systemd {
                start_timeout_seconds,
                stop_grace_seconds,
                abort_timeout_seconds,
            } => ServiceTimeouts::Systemd {
                start_timeout_seconds,
                stop_grace_seconds,
                abort_timeout_seconds,
                settle_seconds: 1,
            },
            Self::Openrc {
                stop_grace_seconds,
                kill_wait_seconds,
            } => ServiceTimeouts::Openrc {
                start_timeout_seconds: 1,
                stop_grace_seconds,
                kill_wait_seconds,
                settle_seconds: 1,
            },
            Self::Scm => ServiceTimeouts::Windows {
                start_observation_seconds: 1,
                stop_observation_seconds: 1,
            },
        };
        policy.validate_for(execution)
    }
}

impl ServiceLifecycle {
    pub fn resolved(self, execution: &ServiceExecution) -> Self {
        Self {
            timeouts: Some(self.timeouts_for(execution)),
            ..self
        }
    }

    pub fn timeouts_for(self, execution: &ServiceExecution) -> ServiceTimeouts {
        self.timeouts
            .unwrap_or_else(|| ServiceTimeouts::for_execution(execution))
    }

    pub fn validate(
        self,
        start: crate::ServiceStart,
        execution: &ServiceExecution,
    ) -> Result<(), &'static str> {
        self.timeouts_for(execution).validate_for(execution)?;
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
    #[case::zero(ServiceTimeouts::Windows { start_observation_seconds: 0, stop_observation_seconds: 1 }, ServiceExecution::WindowsJob { account: WindowsServiceAccount::LocalService })]
    #[case::overflow(ServiceTimeouts::Openrc { start_timeout_seconds: 1, stop_grace_seconds: u32::MAX, kill_wait_seconds: 1, settle_seconds: 1 }, ServiceExecution::OpenrcUser { user: "worker".into(), group: None })]
    #[case::openrc_total(ServiceTimeouts::Openrc { start_timeout_seconds: 1, stop_grace_seconds: 3590, kill_wait_seconds: 5, settle_seconds: 6 }, ServiceExecution::OpenrcUser { user: "worker".into(), group: None })]
    #[case::systemd_total(ServiceTimeouts::Systemd { start_timeout_seconds: 1, stop_grace_seconds: 3590, abort_timeout_seconds: 5, settle_seconds: 6 }, ServiceExecution::SystemdDynamicUser)]
    #[case::start_total(ServiceTimeouts::Openrc { start_timeout_seconds: 3600, stop_grace_seconds: 1, kill_wait_seconds: 1, settle_seconds: 1 }, ServiceExecution::OpenrcUser { user: "worker".into(), group: None })]
    #[case::wrong_manager(ServiceTimeouts::Windows { start_observation_seconds: 1, stop_observation_seconds: 1 }, ServiceExecution::SystemdDynamicUser)]
    fn unenforceable_timeout_policies_are_rejected(
        #[case] timeouts: ServiceTimeouts,
        #[case] execution: ServiceExecution,
    ) {
        assert!(timeouts.validate_for(&execution).is_err());
    }

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
            ServiceExecution::WindowsJob {
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
