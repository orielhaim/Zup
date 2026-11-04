use serde::{Deserialize, Serialize};

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
