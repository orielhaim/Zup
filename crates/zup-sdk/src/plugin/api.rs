use super::exports::zup::plugin::planner::{
    Context as WitContext, InstallScope, InstallationPlan, PluginError,
};
use super::plan::{Error, Plan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    User,
    Machine,
}

impl Scope {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub plugin_id: String,
    pub app_id: String,
    pub app_name: String,
    pub app_version: String,
    pub install_directory: String,
    pub scope: Scope,
    pub target: String,
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

pub trait Plugin {
    fn plan(context: Context) -> Result<Plan, Error>;
}

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
