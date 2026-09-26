//! Static template resolution for planning.

use zup_core::{App, Template, Variable, VariableValue};

use crate::error::PlanError;

/// Resolve `${app.*}` and `${install}` into a concrete template.
///
/// `${location.*}` variables remain for platform resolution.
pub fn resolve_template(
    template: &Template,
    app: &App,
    install_directory: &Template,
) -> Result<Template, PlanError> {
    let resolved = template.substitute(|variable| match variable {
        Variable::AppId => Some(VariableValue::Literal(app.id.to_string())),
        Variable::AppName => Some(VariableValue::Literal(app.name.to_string())),
        Variable::AppVersion => Some(VariableValue::Literal(app.version.to_string())),
        Variable::Install => Some(VariableValue::Template(install_directory.clone())),
        Variable::Location(_) => None,
    });

    debug_assert!(!resolved.contains_variable(Variable::Install));
    debug_assert!(!resolved.contains_variable(Variable::AppId));
    debug_assert!(!resolved.contains_variable(Variable::AppName));
    debug_assert!(!resolved.contains_variable(Variable::AppVersion));
    Ok(resolved)
}

/// Resolve the scope-specific install directory (app variables only).
///
/// The result may still contain `${location.*}`. It must not contain `${install}`.
pub fn resolve_install_directory(directory: &Template, app: &App) -> Result<Template, PlanError> {
    let resolved = directory.substitute(|variable| match variable {
        Variable::AppId => Some(VariableValue::Literal(app.id.to_string())),
        Variable::AppName => Some(VariableValue::Literal(app.name.to_string())),
        Variable::AppVersion => Some(VariableValue::Literal(app.version.to_string())),
        Variable::Install => {
            // Statically rejected at compile; defensive only.
            Some(VariableValue::Literal(String::new()))
        }
        Variable::Location(_) => None,
    });

    if directory.contains_variable(Variable::Install) {
        return Err(PlanError::RecursiveInstallDirectory);
    }
    Ok(resolved)
}
