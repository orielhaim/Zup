//! The question a plugin is asked, and the trait that answers it.

use crate::plan::{Error, Plan};
use crate::planner::{Context as WitContext, InstallScope, InstallationPlan, PluginError};

/// Who the application is installing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The person running the installer.
    User,
    /// Every user of the machine. Whether that is permitted is Zup's decision;
    /// the plugin only learns what was chosen.
    Machine,
}

impl Scope {
    /// Who this is, as the manifest spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Machine => "machine",
        }
    }
}

impl From<Scope> for InstallScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::User => Self::User,
            Scope::Machine => Self::Machine,
        }
    }
}

impl From<InstallScope> for Scope {
    fn from(scope: InstallScope) -> Self {
        match scope {
            InstallScope::User => Self::User,
            InstallScope::Machine => Self::Machine,
        }
    }
}

/// What the host is installing, as a plugin sees it.
///
/// Everything here is a fact about the installation rather than about the
/// machine. There is no way to ask what is already installed, what is running,
/// or where anything else lives: a plugin that needs to know something gets it
/// from the application manifest, which Zup has already checked against the plan
/// this call is producing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// The plugin's own identifier, as the application declared it.
    pub plugin_id: String,
    /// The application being installed.
    pub app_id: String,
    /// Its display name.
    pub app_name: String,
    /// Its version.
    pub app_version: String,
    /// Where it will be installed. A template: `${install}` and the other
    /// location variables are resolved by Zup as it writes each resource.
    pub install_directory: String,
    /// Who it is being installed for.
    pub scope: Scope,
    /// The target triple.
    pub target: String,
    /// The components a person selected, by identifier.
    pub selected_components: Vec<String>,
}

impl From<WitContext> for Context {
    fn from(context: WitContext) -> Self {
        Self {
            plugin_id: context.plugin_id,
            app_id: context.app_id,
            app_name: context.app_name,
            app_version: context.app_version,
            install_directory: context.install_directory,
            scope: context.install_scope.into(),
            target: context.target,
            selected_components: context.selected_components,
        }
    }
}

/// A Zup plugin.
///
/// One method. What it returns is checked against the application's own manifest
/// by the host, so a plan that contradicts what the application declares is
/// refused rather than applied.
pub trait Plugin {
    /// What should exist on the machine once this installation has committed.
    ///
    /// Returning an [`Error`] is a normal answer, not a failure: it says "this
    /// plugin cannot plan for that context", with a code the host can branch on
    /// and a message a person can read.
    fn plan(context: Context) -> Result<Plan, Error>;
}

/// Carry a plugin's answer across the ABI boundary.
///
/// The one place the authoring types and the WIT types meet. Everything a plugin
/// author writes is on one side of this and everything the host links against is
/// on the other, so the conversion has exactly one definition: a change to
/// either shape shows up here rather than in every plugin.
///
/// The module is public only so `plugin_export!` can name this; nothing else
/// should.
pub fn __answer<P: Plugin>() -> impl Fn(WitContext) -> Result<InstallationPlan, PluginError> {
    move |context: WitContext| match P::plan(Context::from(context)) {
        Ok(plan) => {
            plan.validate()?;
            Ok(InstallationPlan {
                resources: plan.into_resources(),
            })
        }
        Err(error) => Err(error.into()),
    }
}